//! Abstract syntax tree produced by the parser and consumed by the engine.

use crate::value::{ColumnType, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Begin,
    Commit,
    Rollback,
    CreateTable {
        name: String,
        columns: Vec<(String, ColumnType)>,
    },
    DropTable {
        name: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    },
    Select(SelectStmt),
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
}

/// A `SELECT` query, possibly with joins, aggregation, ordering, and a limit.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub items: Vec<SelectItem>,
    pub from: String,
    pub joins: Vec<Join>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub order_by: Option<(String, bool)>, // (output column name, ascending)
    pub limit: Option<i64>,
}

/// An `INNER JOIN <table> ON <predicate>` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub table: String,
    pub on: Expr,
}

/// One item in a SELECT projection list.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*` — every column of the (joined) input.
    Wildcard,
    /// An expression with an optional `AS` alias.
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    /// A column reference, optionally qualified by a table name (`t.col`).
    Column {
        table: Option<String>,
        name: String,
    },
    /// An aggregate function. `arg == None` represents `COUNT(*)`.
    Aggregate {
        func: AggFunc,
        arg: Option<Box<Expr>>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    pub fn from_name(s: &str) -> Option<AggFunc> {
        Some(match s.to_ascii_uppercase().as_str() {
            "COUNT" => AggFunc::Count,
            "SUM" => AggFunc::Sum,
            "AVG" => AggFunc::Avg,
            "MIN" => AggFunc::Min,
            "MAX" => AggFunc::Max,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
}
