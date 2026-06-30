//! The execution engine: turns parsed `Statement`s into reads and writes
//! against the catalog and the per-table B+Trees.

use std::cmp::Ordering;
use std::path::Path;

use crate::btree::BTree;
use crate::catalog::{Catalog, TableDef};
use crate::error::{DbError, Result};
use crate::pager::Pager;
use crate::sql::ast::{BinOp, Expr, Projection, Statement, UnOp};
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
    Select {
        columns: Vec<String>,
        rows: Vec<Row>,
    },
}

pub struct Database {
    pager: Pager,
    catalog: Catalog,
}

impl Database {
    /// Open (or create) a database file.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Database> {
        let mut pager = Pager::open(path)?;
        let catalog = Catalog::load(&mut pager)?;
        Ok(Database { pager, catalog })
    }

    /// Parse and execute one or more `;`-separated SQL statements, persisting
    /// all changes to disk at the end.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        let statements = Parser::parse_sql(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for stmt in statements {
            results.push(self.execute_statement(stmt)?);
        }
        self.pager.flush()?;
        Ok(results)
    }

    fn execute_statement(&mut self, stmt: Statement) -> Result<QueryResult> {
        match stmt {
            Statement::CreateTable { name, columns } => self.exec_create(name, columns),
            Statement::DropTable { name } => self.exec_drop(name),
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.exec_insert(table, columns, rows),
            Statement::Select {
                table,
                projection,
                filter,
                order_by,
                limit,
            } => self.exec_select(table, projection, filter, order_by, limit),
            Statement::Update {
                table,
                assignments,
                filter,
            } => self.exec_update(table, assignments, filter),
            Statement::Delete { table, filter } => self.exec_delete(table, filter),
        }
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
                let v = eval_expr(&expr, &[], &def)?;
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

    fn exec_select(
        &mut self,
        table: String,
        projection: Projection,
        filter: Option<Expr>,
        order_by: Option<(String, bool)>,
        limit: Option<i64>,
    ) -> Result<QueryResult> {
        let def = self
            .catalog
            .get(&table)
            .ok_or_else(|| DbError::Exec(format!("no such table '{table}'")))?
            .clone();

        let tree = BTree::open(def.root);
        let raw = tree.scan(&mut self.pager)?;

        // Decode + filter.
        let mut matched: Vec<Row> = Vec::new();
        for (_key, bytes) in raw {
            let row = decode_row(&bytes, def.columns.len())?;
            let keep = match &filter {
                Some(expr) => eval_expr(expr, &row, &def)?.is_truthy(),
                None => true,
            };
            if keep {
                matched.push(row);
            }
        }

        // ORDER BY.
        if let Some((col, asc)) = &order_by {
            let idx = def.column_index(col).ok_or_else(|| {
                DbError::Exec(format!("no such column '{col}' in ORDER BY"))
            })?;
            matched.sort_by(|a, b| {
                let ord = compare_values(&a[idx], &b[idx]).unwrap_or(Ordering::Equal);
                if *asc {
                    ord
                } else {
                    ord.reverse()
                }
            });
        }

        // LIMIT.
        if let Some(n) = limit {
            let n = n.max(0) as usize;
            matched.truncate(n);
        }

        // Projection.
        let (col_names, proj_indices): (Vec<String>, Vec<usize>) = match projection {
            Projection::All => (
                def.columns.iter().map(|(n, _)| n.clone()).collect(),
                (0..def.columns.len()).collect(),
            ),
            Projection::Columns(cols) => {
                let mut names = Vec::new();
                let mut idxs = Vec::new();
                for c in cols {
                    let idx = def.column_index(&c).ok_or_else(|| {
                        DbError::Exec(format!("no such column '{c}' in table '{table}'"))
                    })?;
                    names.push(def.columns[idx].0.clone());
                    idxs.push(idx);
                }
                (names, idxs)
            }
        };

        let rows = matched
            .into_iter()
            .map(|r| proj_indices.iter().map(|i| r[*i].clone()).collect())
            .collect();

        Ok(QueryResult::Select {
            columns: col_names,
            rows,
        })
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

        let mut tree = BTree::open(def.root);
        let raw = tree.scan(&mut self.pager)?;
        let mut count = 0usize;

        for (key, bytes) in raw {
            let mut row = decode_row(&bytes, def.columns.len())?;
            let keep = match &filter {
                Some(expr) => eval_expr(expr, &row, &def)?.is_truthy(),
                None => true,
            };
            if !keep {
                continue;
            }
            for (idx, expr) in &targets {
                let v = eval_expr(expr, &row, &def)?;
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

        let mut to_delete = Vec::new();
        for (key, bytes) in raw {
            let row = decode_row(&bytes, def.columns.len())?;
            let keep = match &filter {
                Some(expr) => eval_expr(expr, &row, &def)?.is_truthy(),
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

fn eval_expr(expr: &Expr, row: &[Value], def: &TableDef) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column(name) => {
            let idx = def
                .column_index(name)
                .ok_or_else(|| DbError::Exec(format!("no such column '{name}'")))?;
            Ok(row
                .get(idx)
                .cloned()
                .ok_or_else(|| DbError::Exec(format!("column '{name}' not bound in this context")))?)
        }
        Expr::Unary { op, expr } => {
            let v = eval_expr(expr, row, def)?;
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
        Expr::Binary { op, left, right } => {
            // Short-circuiting logical operators.
            match op {
                BinOp::And => {
                    let l = eval_expr(left, row, def)?;
                    if !l.is_truthy() {
                        return Ok(Value::Integer(0));
                    }
                    let r = eval_expr(right, row, def)?;
                    return Ok(Value::Integer(if r.is_truthy() { 1 } else { 0 }));
                }
                BinOp::Or => {
                    let l = eval_expr(left, row, def)?;
                    if l.is_truthy() {
                        return Ok(Value::Integer(1));
                    }
                    let r = eval_expr(right, row, def)?;
                    return Ok(Value::Integer(if r.is_truthy() { 1 } else { 0 }));
                }
                _ => {}
            }

            let l = eval_expr(left, row, def)?;
            let r = eval_expr(right, row, def)?;

            match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    Ok(eval_comparison(*op, &l, &r))
                }
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => eval_arithmetic(*op, &l, &r),
                BinOp::And | BinOp::Or => unreachable!("handled above"),
            }
        }
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
