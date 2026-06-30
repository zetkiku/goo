//! The execution engine: turns parsed `Statement`s into reads and writes
//! against the catalog and the per-table B+Trees.

use std::cmp::Ordering;
use std::path::Path;

use crate::btree::BTree;
use crate::catalog::{Catalog, TableDef};
use crate::error::{DbError, Result};
use crate::pager::Pager;
use crate::sql::ast::{AggFunc, BinOp, Expr, SelectItem, SelectStmt, Statement, UnOp};
use crate::sql::Parser;
use crate::value::{decode_row, encode_row, ColumnType, Row, Value};

/// The outcome of executing a single statement.
#[derive(Debug, PartialEq)]
pub enum QueryResult {
    Created(String),
    Dropped(String),
    Inserted(usize),
    Updated(usize),
    Deleted(usize),
    Begin,
    Commit,
    Rollback,
    Select {
        columns: Vec<String>,
        rows: Vec<Row>,
    },
}

pub struct Database {
    pager: Pager,
    catalog: Catalog,
    /// Snapshot of the catalog taken at BEGIN, restored on ROLLBACK.
    saved_catalog: Option<Catalog>,
}

impl Database {
    /// Open (or create) a database file.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Database> {
        let mut pager = Pager::open(path)?;
        let catalog = Catalog::load(&mut pager)?;
        Ok(Database {
            pager,
            catalog,
            saved_catalog: None,
        })
    }

    /// Parse and execute one or more `;`-separated SQL statements. Outside of
    /// an explicit transaction this auto-commits (flushes) at the end of the
    /// batch; inside a transaction, changes are held until COMMIT.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        let statements = Parser::parse_sql(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for stmt in statements {
            results.push(self.execute_statement(stmt)?);
        }
        if !self.pager.in_transaction() {
            self.pager.flush()?;
        }
        Ok(results)
    }

    fn execute_statement(&mut self, stmt: Statement) -> Result<QueryResult> {
        match stmt {
            Statement::Begin => self.exec_begin(),
            Statement::Commit => self.exec_commit(),
            Statement::Rollback => self.exec_rollback(),
            Statement::CreateTable { name, columns } => self.exec_create(name, columns),
            Statement::DropTable { name } => self.exec_drop(name),
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.exec_insert(table, columns, rows),
            Statement::Select(select) => self.exec_select(select),
            Statement::Update {
                table,
                assignments,
                filter,
            } => self.exec_update(table, assignments, filter),
            Statement::Delete { table, filter } => self.exec_delete(table, filter),
        }
    }

    // --- transactions ------------------------------------------------------

    fn exec_begin(&mut self) -> Result<QueryResult> {
        if self.pager.in_transaction() {
            return Err(DbError::Exec("a transaction is already in progress".into()));
        }
        self.pager.begin_transaction();
        self.saved_catalog = Some(self.catalog.clone());
        Ok(QueryResult::Begin)
    }

    fn exec_commit(&mut self) -> Result<QueryResult> {
        if !self.pager.in_transaction() {
            return Err(DbError::Exec("no transaction in progress".into()));
        }
        self.pager.commit_transaction()?;
        self.saved_catalog = None;
        Ok(QueryResult::Commit)
    }

    fn exec_rollback(&mut self) -> Result<QueryResult> {
        if !self.pager.in_transaction() {
            return Err(DbError::Exec("no transaction in progress".into()));
        }
        self.pager.rollback_transaction();
        if let Some(saved) = self.saved_catalog.take() {
            self.catalog = saved;
        }
        Ok(QueryResult::Rollback)
    }

    // --- DDL ---------------------------------------------------------------

    fn exec_create(
        &mut self,
        name: String,
        columns: Vec<(String, ColumnType)>,
    ) -> Result<QueryResult> {
        if self.catalog.contains(&name) {
            return Err(DbError::Exec(format!("table '{name}' already exists")));
        }
        if columns.is_empty() {
            return Err(DbError::Exec("table must have at least one column".into()));
        }
        // Reject duplicate column names.
        for i in 0..columns.len() {
            for j in (i + 1)..columns.len() {
                if columns[i].0.eq_ignore_ascii_case(&columns[j].0) {
                    return Err(DbError::Exec(format!(
                        "duplicate column name '{}'",
                        columns[i].0
                    )));
                }
            }
        }
        let tree = BTree::create(&mut self.pager)?;
        self.catalog.insert(TableDef {
            name: name.clone(),
            root: tree.root,
            next_rowid: 1,
            columns,
        });
        self.catalog.save(&mut self.pager)?;
        Ok(QueryResult::Created(name))
    }

    fn exec_drop(&mut self, name: String) -> Result<QueryResult> {
        if self.catalog.remove(&name).is_none() {
            return Err(DbError::Exec(format!("table '{name}' does not exist")));
        }
        // Note: pages of the dropped table are intentionally not reclaimed here;
        // a full implementation would walk and free them.
        self.catalog.save(&mut self.pager)?;
        Ok(QueryResult::Dropped(name))
    }

    // --- INSERT ------------------------------------------------------------

    fn exec_insert(
        &mut self,
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    ) -> Result<QueryResult> {
        let mut def = self
            .catalog
            .get(&table)
            .ok_or_else(|| DbError::Exec(format!("no such table '{table}'")))?
            .clone();

        // Map the provided column order to schema indices.
        let target_indices: Vec<usize> = match &columns {
            Some(cols) => {
                let mut idxs = Vec::with_capacity(cols.len());
                for c in cols {
                    let idx = def.column_index(c).ok_or_else(|| {
                        DbError::Exec(format!("no such column '{c}' in table '{table}'"))
                    })?;
                    idxs.push(idx);
                }
                idxs
            }
            None => (0..def.columns.len()).collect(),
        };

        let mut tree = BTree::open(def.root);
        let mut count = 0usize;

        for value_exprs in rows {
            if value_exprs.len() != target_indices.len() {
                return Err(DbError::Exec(format!(
                    "INSERT has {} values but {} columns were targeted",
                    value_exprs.len(),
                    target_indices.len()
                )));
            }
            // Start with all NULLs, then fill targeted columns.
            let mut row: Row = vec![Value::Null; def.columns.len()];
            for (slot, expr) in target_indices.iter().zip(value_exprs.into_iter()) {
                // INSERT values are constant expressions (no column refs).
                let v = eval_expr(&expr, &[], &QSchema::empty())?;
                let coerced = coerce(v, def.columns[*slot].1, &def.columns[*slot].0)?;
                row[*slot] = coerced;
            }
            let rowid = def.next_rowid;
            def.next_rowid += 1;
            tree.insert(&mut self.pager, rowid, encode_row(&row))?;
            count += 1;
        }

        def.root = tree.root;
        self.catalog.insert(def);
        self.catalog.save(&mut self.pager)?;
        Ok(QueryResult::Inserted(count))
    }

    // --- SELECT ------------------------------------------------------------

    fn exec_select(&mut self, stmt: SelectStmt) -> Result<QueryResult> {
        // 1. Materialize the FROM clause (base table + any INNER JOINs) into a
        //    flat (schema, rows) input set.
        let base = self
            .catalog
            .get(&stmt.from)
            .ok_or_else(|| DbError::Exec(format!("no such table '{}'", stmt.from)))?
            .clone();
        let mut schema = QSchema::from_table(&base);
        let mut rows: Vec<Row> = BTree::open(base.root)
            .scan(&mut self.pager)?
            .into_iter()
            .map(|(_k, bytes)| decode_row(&bytes, base.columns.len()))
            .collect::<Result<_>>()?;

        for join in &stmt.joins {
            let jdef = self
                .catalog
                .get(&join.table)
                .ok_or_else(|| DbError::Exec(format!("no such table '{}'", join.table)))?
                .clone();
            let jrows: Vec<Row> = BTree::open(jdef.root)
                .scan(&mut self.pager)?
                .into_iter()
                .map(|(_k, bytes)| decode_row(&bytes, jdef.columns.len()))
                .collect::<Result<_>>()?;

            let joined_schema = schema.concat(&QSchema::from_table(&jdef));
            check_columns(&join.on, &joined_schema)?;
            let mut joined_rows = Vec::new();
            for left in &rows {
                for right in &jrows {
                    let mut combined = left.clone();
                    combined.extend(right.iter().cloned());
                    if eval_expr(&join.on, &combined, &joined_schema)?.is_truthy() {
                        joined_rows.push(combined);
                    }
                }
            }
            schema = joined_schema;
            rows = joined_rows;
        }

        // Validate every referenced column up front, so that errors surface
        // even when the input set is empty.
        if let Some(filter) = &stmt.filter {
            check_columns(filter, &schema)?;
        }
        for g in &stmt.group_by {
            check_columns(g, &schema)?;
        }
        for it in &stmt.items {
            if let SelectItem::Expr { expr, .. } = it {
                check_columns(expr, &schema)?;
            }
        }

        // 2. WHERE.
        if let Some(filter) = &stmt.filter {
            let mut kept = Vec::with_capacity(rows.len());
            for row in rows {
                if eval_expr(filter, &row, &schema)?.is_truthy() {
                    kept.push(row);
                }
            }
            rows = kept;
        }

        // 3. Projection — aggregate vs. row-wise.
        let aggregate_mode = !stmt.group_by.is_empty()
            || stmt.items.iter().any(|it| match it {
                SelectItem::Expr { expr, .. } => expr_has_aggregate(expr),
                SelectItem::Wildcard => false,
            });

        let (columns, mut out_rows) = if aggregate_mode {
            self.project_aggregate(&stmt, &schema, rows)?
        } else {
            project_rows(&stmt.items, &schema, rows)?
        };

        // 4. ORDER BY (against the output columns).
        if let Some((col, asc)) = &stmt.order_by {
            let idx = columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(col))
                .ok_or_else(|| {
                    DbError::Exec(format!("no such output column '{col}' in ORDER BY"))
                })?;
            out_rows.sort_by(|a, b| {
                let ord = compare_values(&a[idx], &b[idx]).unwrap_or(Ordering::Equal);
                if *asc {
                    ord
                } else {
                    ord.reverse()
                }
            });
        }

        // 5. LIMIT.
        if let Some(n) = stmt.limit {
            out_rows.truncate(n.max(0) as usize);
        }

        Ok(QueryResult::Select {
            columns,
            rows: out_rows,
        })
    }

    /// Compute grouped/aggregated output rows.
    fn project_aggregate(
        &self,
        stmt: &SelectStmt,
        schema: &QSchema,
        rows: Vec<Row>,
    ) -> Result<(Vec<String>, Vec<Row>)> {
        for it in &stmt.items {
            if matches!(it, SelectItem::Wildcard) {
                return Err(DbError::Exec(
                    "'*' cannot be combined with aggregates or GROUP BY".into(),
                ));
            }
        }

        // Partition rows into groups keyed by the GROUP BY expression values,
        // preserving first-seen order.
        let mut keys: Vec<Vec<Value>> = Vec::new();
        let mut groups: Vec<Vec<Row>> = Vec::new();
        if stmt.group_by.is_empty() {
            groups.push(rows);
            keys.push(Vec::new());
        } else {
            for row in rows {
                let key: Vec<Value> = stmt
                    .group_by
                    .iter()
                    .map(|e| eval_expr(e, &row, schema))
                    .collect::<Result<_>>()?;
                match keys.iter().position(|k| k == &key) {
                    Some(i) => groups[i].push(row),
                    None => {
                        keys.push(key);
                        groups.push(vec![row]);
                    }
                }
            }
        }

        let columns: Vec<String> = stmt
            .items
            .iter()
            .map(|it| match it {
                SelectItem::Expr { expr, alias } => {
                    alias.clone().unwrap_or_else(|| derived_name(expr))
                }
                SelectItem::Wildcard => unreachable!(),
            })
            .collect();

        let mut out_rows = Vec::with_capacity(groups.len());
        for group in &groups {
            let mut out = Vec::with_capacity(stmt.items.len());
            for it in &stmt.items {
                if let SelectItem::Expr { expr, .. } = it {
                    out.push(eval_grouped(expr, group, schema)?);
                }
            }
            out_rows.push(out);
        }
        Ok((columns, out_rows))
    }

    // --- UPDATE ------------------------------------------------------------

    fn exec_update(
        &mut self,
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
    ) -> Result<QueryResult> {
        let def = self
            .catalog
            .get(&table)
            .ok_or_else(|| DbError::Exec(format!("no such table '{table}'")))?
            .clone();

        // Resolve assignment targets up front.
        let mut targets = Vec::with_capacity(assignments.len());
        for (col, expr) in &assignments {
            let idx = def
                .column_index(col)
                .ok_or_else(|| DbError::Exec(format!("no such column '{col}'")))?;
            targets.push((idx, expr.clone()));
        }

        let schema = QSchema::from_table(&def);
        let mut tree = BTree::open(def.root);
        let raw = tree.scan(&mut self.pager)?;
        let mut count = 0usize;

        for (key, bytes) in raw {
            let mut row = decode_row(&bytes, def.columns.len())?;
            let keep = match &filter {
                Some(expr) => eval_expr(expr, &row, &schema)?.is_truthy(),
                None => true,
            };
            if !keep {
                continue;
            }
            for (idx, expr) in &targets {
                let v = eval_expr(expr, &row, &schema)?;
                row[*idx] = coerce(v, def.columns[*idx].1, &def.columns[*idx].0)?;
            }
            tree.insert(&mut self.pager, key, encode_row(&row))?;
            count += 1;
        }

        // root may have changed if an updated row grew and triggered a split.
        let mut def = def;
        def.root = tree.root;
        self.catalog.insert(def);
        self.catalog.save(&mut self.pager)?;
        Ok(QueryResult::Updated(count))
    }

    // --- DELETE ------------------------------------------------------------

    fn exec_delete(&mut self, table: String, filter: Option<Expr>) -> Result<QueryResult> {
        let def = self
            .catalog
            .get(&table)
            .ok_or_else(|| DbError::Exec(format!("no such table '{table}'")))?
            .clone();

        let mut tree = BTree::open(def.root);
        let raw = tree.scan(&mut self.pager)?;

        let schema = QSchema::from_table(&def);
        let mut to_delete = Vec::new();
        for (key, bytes) in raw {
            let row = decode_row(&bytes, def.columns.len())?;
            let keep = match &filter {
                Some(expr) => eval_expr(expr, &row, &schema)?.is_truthy(),
                None => true,
            };
            if keep {
                to_delete.push(key);
            }
        }

        let mut count = 0;
        for key in to_delete {
            if tree.delete(&mut self.pager, key)? {
                count += 1;
            }
        }
        self.catalog.save(&mut self.pager)?;
        Ok(QueryResult::Deleted(count))
    }

    pub fn table_names(&self) -> Vec<String> {
        self.catalog.table_names()
    }
}

// ---------------------------------------------------------------------------
// Expression evaluation
// ---------------------------------------------------------------------------

/// Coerce a value to a column's declared type (NULL is always allowed).
fn coerce(v: Value, ty: ColumnType, col: &str) -> Result<Value> {
    match (&v, ty) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Integer(_), ColumnType::Integer) => Ok(v),
        (Value::Text(_), ColumnType::Text) => Ok(v),
        (got, want) => Err(DbError::Exec(format!(
            "type mismatch for column '{col}': expected {}, got {}",
            want.name(),
            got.type_name()
        ))),
    }
}

/// A query-time schema: an ordered list of (optional table qualifier, column
/// name) describing the columns of an input row (possibly spanning joins).
struct QSchema {
    cols: Vec<(Option<String>, String)>,
}

impl QSchema {
    fn empty() -> QSchema {
        QSchema { cols: Vec::new() }
    }

    fn from_table(def: &TableDef) -> QSchema {
        QSchema {
            cols: def
                .columns
                .iter()
                .map(|(n, _)| (Some(def.name.clone()), n.clone()))
                .collect(),
        }
    }

    fn concat(&self, other: &QSchema) -> QSchema {
        let mut cols = self.cols.clone();
        cols.extend(other.cols.iter().cloned());
        QSchema { cols }
    }

    /// Resolve a (possibly qualified) column reference to a row index.
    fn resolve(&self, table: &Option<String>, name: &str) -> Result<usize> {
        let mut found: Option<usize> = None;
        for (i, (t, n)) in self.cols.iter().enumerate() {
            let name_ok = n.eq_ignore_ascii_case(name);
            let table_ok = match table {
                Some(q) => t.as_deref().is_some_and(|tt| tt.eq_ignore_ascii_case(q)),
                None => true,
            };
            if name_ok && table_ok {
                if found.is_some() {
                    return Err(DbError::Exec(format!("ambiguous column '{name}'")));
                }
                found = Some(i);
            }
        }
        found.ok_or_else(|| {
            let q = table.as_ref().map(|t| format!("{t}.")).unwrap_or_default();
            DbError::Exec(format!("no such column '{q}{name}'"))
        })
    }
}

/// Statically verify that every column referenced in an expression resolves
/// against the schema (so errors surface even when no rows are present).
fn check_columns(expr: &Expr, schema: &QSchema) -> Result<()> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column { table, name } => schema.resolve(table, name).map(|_| ()),
        Expr::Aggregate { arg, .. } => {
            if let Some(a) = arg {
                check_columns(a, schema)?;
            }
            Ok(())
        }
        Expr::Unary { expr, .. } => check_columns(expr, schema),
        Expr::Binary { left, right, .. } => {
            check_columns(left, schema)?;
            check_columns(right, schema)
        }
    }
}

/// Does this expression contain an aggregate anywhere inside it?
fn expr_has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate { .. } => true,
        Expr::Unary { expr, .. } => expr_has_aggregate(expr),
        Expr::Binary { left, right, .. } => expr_has_aggregate(left) || expr_has_aggregate(right),
        _ => false,
    }
}

/// A reasonable default output-column name for an unaliased expression.
fn derived_name(expr: &Expr) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        Expr::Aggregate { func, .. } => func.name().to_string(),
        _ => "expr".to_string(),
    }
}

/// Row-wise (non-aggregate) projection.
fn project_rows(
    items: &[SelectItem],
    schema: &QSchema,
    rows: Vec<Row>,
) -> Result<(Vec<String>, Vec<Row>)> {
    // Build the output column names once.
    let mut columns = Vec::new();
    for it in items {
        match it {
            SelectItem::Wildcard => {
                for (_, name) in &schema.cols {
                    columns.push(name.clone());
                }
            }
            SelectItem::Expr { expr, alias } => {
                columns.push(alias.clone().unwrap_or_else(|| derived_name(expr)));
            }
        }
    }

    let mut out_rows = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut out = Vec::with_capacity(columns.len());
        for it in items {
            match it {
                SelectItem::Wildcard => out.extend(row.iter().cloned()),
                SelectItem::Expr { expr, .. } => out.push(eval_expr(expr, row, schema)?),
            }
        }
        out_rows.push(out);
    }
    Ok((columns, out_rows))
}

/// Scalar expression evaluation against a single row and schema.
/// Aggregates are not valid here.
fn eval_expr(expr: &Expr, row: &[Value], schema: &QSchema) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column { table, name } => {
            let idx = schema.resolve(table, name)?;
            row.get(idx)
                .cloned()
                .ok_or_else(|| DbError::Exec(format!("column '{name}' not bound in this context")))
        }
        Expr::Aggregate { .. } => Err(DbError::Exec(
            "aggregate functions are not allowed here".into(),
        )),
        Expr::Unary { op, expr } => {
            let v = eval_expr(expr, row, schema)?;
            eval_unary(*op, v)
        }
        Expr::Binary { op, left, right } => match op {
            BinOp::And => {
                let l = eval_expr(left, row, schema)?;
                if !l.is_truthy() {
                    return Ok(Value::Integer(0));
                }
                let r = eval_expr(right, row, schema)?;
                Ok(Value::Integer(if r.is_truthy() { 1 } else { 0 }))
            }
            BinOp::Or => {
                let l = eval_expr(left, row, schema)?;
                if l.is_truthy() {
                    return Ok(Value::Integer(1));
                }
                let r = eval_expr(right, row, schema)?;
                Ok(Value::Integer(if r.is_truthy() { 1 } else { 0 }))
            }
            _ => {
                let l = eval_expr(left, row, schema)?;
                let r = eval_expr(right, row, schema)?;
                eval_binary(*op, &l, &r)
            }
        },
    }
}

/// Evaluate an expression in the context of a group of rows. Aggregates fold
/// over the whole group; non-aggregate sub-expressions use a representative
/// (first) row of the group.
fn eval_grouped(expr: &Expr, group: &[Row], schema: &QSchema) -> Result<Value> {
    match expr {
        Expr::Aggregate { func, arg } => compute_aggregate(*func, arg, group, schema),
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column { table, name } => {
            let idx = schema.resolve(table, name)?;
            match group.first() {
                Some(row) => Ok(row.get(idx).cloned().unwrap_or(Value::Null)),
                None => Ok(Value::Null),
            }
        }
        Expr::Unary { op, expr } => {
            let v = eval_grouped(expr, group, schema)?;
            eval_unary(*op, v)
        }
        Expr::Binary { op, left, right } => {
            let l = eval_grouped(left, group, schema)?;
            let r = eval_grouped(right, group, schema)?;
            match op {
                BinOp::And => Ok(Value::Integer(if l.is_truthy() && r.is_truthy() {
                    1
                } else {
                    0
                })),
                BinOp::Or => Ok(Value::Integer(if l.is_truthy() || r.is_truthy() {
                    1
                } else {
                    0
                })),
                _ => eval_binary(*op, &l, &r),
            }
        }
    }
}

/// Fold an aggregate function over a group of rows.
fn compute_aggregate(
    func: AggFunc,
    arg: &Option<Box<Expr>>,
    group: &[Row],
    schema: &QSchema,
) -> Result<Value> {
    // COUNT(*) counts rows directly.
    if func == AggFunc::Count && arg.is_none() {
        return Ok(Value::Integer(group.len() as i64));
    }
    let arg = arg
        .as_ref()
        .ok_or_else(|| DbError::Exec("aggregate requires an argument".into()))?;

    // Evaluate the argument for each row, dropping NULLs.
    let mut values = Vec::new();
    for row in group {
        let v = eval_expr(arg, row, schema)?;
        if v != Value::Null {
            values.push(v);
        }
    }

    match func {
        AggFunc::Count => Ok(Value::Integer(values.len() as i64)),
        AggFunc::Sum | AggFunc::Avg => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let mut sum: i64 = 0;
            for v in &values {
                match v {
                    Value::Integer(n) => sum = sum.wrapping_add(*n),
                    other => {
                        return Err(DbError::Exec(format!(
                            "{}() requires integer values, got {}",
                            func.name().to_uppercase(),
                            other.type_name()
                        )))
                    }
                }
            }
            if func == AggFunc::Sum {
                Ok(Value::Integer(sum))
            } else {
                Ok(Value::Integer(sum / values.len() as i64))
            }
        }
        AggFunc::Min | AggFunc::Max => {
            let mut best: Option<&Value> = None;
            for v in &values {
                best = Some(match best {
                    None => v,
                    Some(cur) => {
                        let ord = compare_values(v, cur).unwrap_or(Ordering::Equal);
                        let take = if func == AggFunc::Min {
                            ord == Ordering::Less
                        } else {
                            ord == Ordering::Greater
                        };
                        if take {
                            v
                        } else {
                            cur
                        }
                    }
                });
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        }
    }
}

fn eval_unary(op: UnOp, v: Value) -> Result<Value> {
    match op {
        UnOp::Neg => match v {
            Value::Integer(n) => Ok(Value::Integer(-n)),
            Value::Null => Ok(Value::Null),
            other => Err(DbError::Exec(format!(
                "cannot negate {}",
                other.type_name()
            ))),
        },
        UnOp::Not => Ok(Value::Integer(if v.is_truthy() { 0 } else { 1 })),
    }
}

fn eval_binary(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    match op {
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            Ok(eval_comparison(op, l, r))
        }
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => eval_arithmetic(op, l, r),
        BinOp::And | BinOp::Or => Ok(Value::Integer(if l.is_truthy() && r.is_truthy() {
            1
        } else {
            0
        })),
    }
}

fn eval_comparison(op: BinOp, l: &Value, r: &Value) -> Value {
    // NULL or incomparable operands -> NULL (treated as false by is_truthy).
    let ord = match compare_values(l, r) {
        Some(o) => o,
        None => return Value::Null,
    };
    let result = match op {
        BinOp::Eq => ord == Ordering::Equal,
        BinOp::Ne => ord != Ordering::Equal,
        BinOp::Lt => ord == Ordering::Less,
        BinOp::Le => ord != Ordering::Greater,
        BinOp::Gt => ord == Ordering::Greater,
        BinOp::Ge => ord != Ordering::Less,
        _ => unreachable!(),
    };
    Value::Integer(if result { 1 } else { 0 })
}

fn eval_arithmetic(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    match (l, r) {
        (Value::Integer(a), Value::Integer(b)) => {
            let v = match op {
                BinOp::Add => a.wrapping_add(*b),
                BinOp::Sub => a.wrapping_sub(*b),
                BinOp::Mul => a.wrapping_mul(*b),
                BinOp::Div => {
                    if *b == 0 {
                        return Ok(Value::Null); // division by zero -> NULL
                    }
                    a / b
                }
                _ => unreachable!(),
            };
            Ok(Value::Integer(v))
        }
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        _ => Err(DbError::Exec(
            "arithmetic is only supported on integers".into(),
        )),
    }
}

/// Compare two values. Returns None for NULL operands or incompatible types.
fn compare_values(l: &Value, r: &Value) -> Option<Ordering> {
    match (l, r) {
        (Value::Integer(a), Value::Integer(b)) => Some(a.cmp(b)),
        (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
        _ => None,
    }
}
