//! Recursive-descent statement parser with a Pratt (precedence-climbing)
//! expression sub-parser.

use crate::error::{DbError, Result};
use crate::sql::ast::*;
use crate::sql::token::{Keyword, Lexer, Token};
use crate::value::{ColumnType, Value};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Parser {
        Parser { tokens, pos: 0 }
    }

    /// Parse a full source string into zero or more statements.
    pub fn parse_sql(src: &str) -> Result<Vec<Statement>> {
        let tokens = Lexer::new(src).tokenize()?;
        let mut parser = Parser::new(tokens);
        parser.parse_program()
    }

    fn parse_program(&mut self) -> Result<Vec<Statement>> {
        let mut stmts = Vec::new();
        loop {
            // Skip any stray semicolons between statements.
            while self.peek() == &Token::Semicolon {
                self.advance();
            }
            if self.peek() == &Token::Eof {
                break;
            }
            stmts.push(self.parse_statement()?);
            match self.peek() {
                Token::Semicolon => {
                    self.advance();
                }
                Token::Eof => break,
                other => {
                    return Err(DbError::Parse(format!(
                        "expected ';' or end of input, found {other:?}"
                    )))
                }
            }
        }
        Ok(stmts)
    }

    // --- token helpers -----------------------------------------------------

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        t
    }

    fn expect(&mut self, t: &Token) -> Result<()> {
        if self.peek() == t {
            self.advance();
            Ok(())
        } else {
            Err(DbError::Parse(format!(
                "expected {t:?}, found {:?}",
                self.peek()
            )))
        }
    }

    fn expect_keyword(&mut self, kw: Keyword) -> Result<()> {
        match self.peek() {
            Token::Keyword(k) if *k == kw => {
                self.advance();
                Ok(())
            }
            other => Err(DbError::Parse(format!("expected {kw:?}, found {other:?}"))),
        }
    }

    fn eat_keyword(&mut self, kw: Keyword) -> bool {
        if matches!(self.peek(), Token::Keyword(k) if *k == kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_identifier(&mut self) -> Result<String> {
        match self.advance() {
            Token::Identifier(s) => Ok(s),
            other => Err(DbError::Parse(format!(
                "expected identifier, found {other:?}"
            ))),
        }
    }

    // --- statements --------------------------------------------------------

    fn parse_statement(&mut self) -> Result<Statement> {
        match self.peek() {
            Token::Keyword(Keyword::Begin) => {
                self.advance();
                self.eat_keyword(Keyword::Transaction);
                Ok(Statement::Begin)
            }
            Token::Keyword(Keyword::Commit) => {
                self.advance();
                self.eat_keyword(Keyword::Transaction);
                Ok(Statement::Commit)
            }
            Token::Keyword(Keyword::Rollback) => {
                self.advance();
                self.eat_keyword(Keyword::Transaction);
                Ok(Statement::Rollback)
            }
            Token::Keyword(Keyword::Create) => self.parse_create(),
            Token::Keyword(Keyword::Drop) => self.parse_drop(),
            Token::Keyword(Keyword::Insert) => self.parse_insert(),
            Token::Keyword(Keyword::Select) => self.parse_select(),
            Token::Keyword(Keyword::Update) => self.parse_update(),
            Token::Keyword(Keyword::Delete) => self.parse_delete(),
            other => Err(DbError::Parse(format!(
                "unexpected start of statement: {other:?}"
            ))),
        }
    }

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Create)?;
        self.expect_keyword(Keyword::Table)?;
        let name = self.expect_identifier()?;
        self.expect(&Token::LParen)?;
        let mut columns = Vec::new();
        loop {
            let col = self.expect_identifier()?;
            let ty = match self.advance() {
                Token::Keyword(Keyword::Integer) => ColumnType::Integer,
                Token::Keyword(Keyword::Text) => ColumnType::Text,
                other => {
                    return Err(DbError::Parse(format!(
                        "expected column type for '{col}', found {other:?}"
                    )))
                }
            };
            columns.push((col, ty));
            match self.peek() {
                Token::Comma => {
                    self.advance();
                }
                Token::RParen => break,
                other => {
                    return Err(DbError::Parse(format!(
                        "expected ',' or ')' in column list, found {other:?}"
                    )))
                }
            }
        }
        self.expect(&Token::RParen)?;
        Ok(Statement::CreateTable { name, columns })
    }

    fn parse_drop(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Drop)?;
        self.expect_keyword(Keyword::Table)?;
        let name = self.expect_identifier()?;
        Ok(Statement::DropTable { name })
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Insert)?;
        self.expect_keyword(Keyword::Into)?;
        let table = self.expect_identifier()?;

        let columns = if self.peek() == &Token::LParen {
            self.advance();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_identifier()?);
                match self.advance() {
                    Token::Comma => continue,
                    Token::RParen => break,
                    other => {
                        return Err(DbError::Parse(format!(
                            "expected ',' or ')' in column list, found {other:?}"
                        )))
                    }
                }
            }
            Some(cols)
        } else {
            None
        };

        self.expect_keyword(Keyword::Values)?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Token::LParen)?;
            let mut values = Vec::new();
            if self.peek() != &Token::RParen {
                loop {
                    values.push(self.parse_expr(0)?);
                    match self.peek() {
                        Token::Comma => {
                            self.advance();
                        }
                        _ => break,
                    }
                }
            }
            self.expect(&Token::RParen)?;
            rows.push(values);
            if self.peek() == &Token::Comma {
                self.advance();
            } else {
                break;
            }
        }
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn parse_select(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Select)?;

        // Projection: a comma-separated list of `*` or `<expr> [AS alias]`.
        let mut items = Vec::new();
        loop {
            if self.peek() == &Token::Star {
                self.advance();
                items.push(SelectItem::Wildcard);
            } else {
                let expr = self.parse_expr(0)?;
                let alias = if self.eat_keyword(Keyword::As) {
                    Some(self.expect_identifier()?)
                } else {
                    None
                };
                items.push(SelectItem::Expr { expr, alias });
            }
            if self.peek() == &Token::Comma {
                self.advance();
            } else {
                break;
            }
        }

        self.expect_keyword(Keyword::From)?;
        let from = self.expect_identifier()?;

        // Zero or more `[INNER] JOIN <table> ON <expr>` clauses.
        let mut joins = Vec::new();
        loop {
            self.eat_keyword(Keyword::Inner);
            if self.eat_keyword(Keyword::Join) {
                let table = self.expect_identifier()?;
                self.expect_keyword(Keyword::On)?;
                let on = self.parse_expr(0)?;
                joins.push(Join { table, on });
            } else {
                break;
            }
        }

        let filter = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        let group_by = if self.eat_keyword(Keyword::Group) {
            self.expect_keyword(Keyword::By)?;
            let mut exprs = Vec::new();
            loop {
                exprs.push(self.parse_expr(0)?);
                if self.peek() == &Token::Comma {
                    self.advance();
                } else {
                    break;
                }
            }
            exprs
        } else {
            Vec::new()
        };

        let order_by = if self.eat_keyword(Keyword::Order) {
            self.expect_keyword(Keyword::By)?;
            let col = self.parse_order_column()?;
            let asc = if self.eat_keyword(Keyword::Desc) {
                false
            } else {
                self.eat_keyword(Keyword::Asc);
                true
            };
            Some((col, asc))
        } else {
            None
        };

        let limit = if self.eat_keyword(Keyword::Limit) {
            match self.advance() {
                Token::Integer(n) => Some(n),
                other => {
                    return Err(DbError::Parse(format!(
                        "expected integer after LIMIT, found {other:?}"
                    )))
                }
            }
        } else {
            None
        };

        Ok(Statement::Select(SelectStmt {
            items,
            from,
            joins,
            filter,
            group_by,
            order_by,
            limit,
        }))
    }

    /// Parse an ORDER BY column, accepting an optional table qualifier.
    /// Ordering matches against output column names, so we keep the final
    /// (column) segment.
    fn parse_order_column(&mut self) -> Result<String> {
        let first = self.expect_identifier()?;
        if self.peek() == &Token::Dot {
            self.advance();
            self.expect_identifier()
        } else {
            Ok(first)
        }
    }

    fn parse_update(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Update)?;
        let table = self.expect_identifier()?;
        self.expect_keyword(Keyword::Set)?;
        let mut assignments = Vec::new();
        loop {
            let col = self.expect_identifier()?;
            self.expect(&Token::Eq)?;
            let value = self.parse_expr(0)?;
            assignments.push((col, value));
            if self.peek() == &Token::Comma {
                self.advance();
            } else {
                break;
            }
        }
        let filter = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Statement::Update {
            table,
            assignments,
            filter,
        })
    }

    fn parse_delete(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Delete)?;
        self.expect_keyword(Keyword::From)?;
        let table = self.expect_identifier()?;
        let filter = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Statement::Delete { table, filter })
    }

    // --- Pratt expression parser ------------------------------------------

    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr> {
        let mut left = self.parse_prefix()?;

        loop {
            let op = match self.peek() {
                Token::Keyword(Keyword::Or) => BinOp::Or,
                Token::Keyword(Keyword::And) => BinOp::And,
                Token::Eq => BinOp::Eq,
                Token::Ne => BinOp::Ne,
                Token::Lt => BinOp::Lt,
                Token::Le => BinOp::Le,
                Token::Gt => BinOp::Gt,
                Token::Ge => BinOp::Ge,
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                _ => break,
            };
            let (lbp, rbp) = infix_binding_power(op);
            if lbp < min_bp {
                break;
            }
            self.advance(); // consume operator
            let right = self.parse_expr(rbp)?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_prefix(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Token::Minus => {
                self.advance();
                let expr = self.parse_expr(9)?;
                Ok(Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(expr),
                })
            }
            Token::Keyword(Keyword::Not) => {
                self.advance();
                let expr = self.parse_expr(3)?;
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(expr),
                })
            }
            Token::LParen => {
                self.advance();
                let e = self.parse_expr(0)?;
                self.expect(&Token::RParen)?;
                Ok(e)
            }
            Token::Integer(n) => {
                self.advance();
                Ok(Expr::Literal(Value::Integer(n)))
            }
            Token::Text(s) => {
                self.advance();
                Ok(Expr::Literal(Value::Text(s)))
            }
            Token::Keyword(Keyword::Null) => {
                self.advance();
                Ok(Expr::Literal(Value::Null))
            }
            Token::Keyword(Keyword::True) => {
                self.advance();
                Ok(Expr::Literal(Value::Integer(1)))
            }
            Token::Keyword(Keyword::False) => {
                self.advance();
                Ok(Expr::Literal(Value::Integer(0)))
            }
            Token::Identifier(name) => {
                self.advance();
                // Aggregate / function call:  NAME ( ... )
                if self.peek() == &Token::LParen {
                    return self.parse_function_call(name);
                }
                // Qualified column:  table . column
                if self.peek() == &Token::Dot {
                    self.advance();
                    let col = self.expect_identifier()?;
                    return Ok(Expr::Column {
                        table: Some(name),
                        name: col,
                    });
                }
                Ok(Expr::Column { table: None, name })
            }
            other => Err(DbError::Parse(format!(
                "unexpected token in expression: {other:?}"
            ))),
        }
    }

    /// Parse a function call once the function name and `(` lookahead are known.
    /// Only aggregate functions are supported.
    fn parse_function_call(&mut self, name: String) -> Result<Expr> {
        let func = AggFunc::from_name(&name)
            .ok_or_else(|| DbError::Parse(format!("unknown function '{name}'")))?;
        self.expect(&Token::LParen)?;
        // COUNT(*) is the only place `*` is allowed as an argument.
        if self.peek() == &Token::Star {
            self.advance();
            self.expect(&Token::RParen)?;
            if func != AggFunc::Count {
                return Err(DbError::Parse(format!(
                    "{}(*) is not supported; only COUNT(*)",
                    name.to_ascii_uppercase()
                )));
            }
            return Ok(Expr::Aggregate { func, arg: None });
        }
        let arg = self.parse_expr(0)?;
        self.expect(&Token::RParen)?;
        Ok(Expr::Aggregate {
            func,
            arg: Some(Box::new(arg)),
        })
    }
}

/// Left/right binding powers. Higher binds tighter; the gap encodes
/// left-associativity (right bp = left bp + 1).
fn infix_binding_power(op: BinOp) -> (u8, u8) {
    match op {
        BinOp::Or => (1, 2),
        BinOp::And => (3, 4),
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => (5, 6),
        BinOp::Add | BinOp::Sub => (7, 8),
        BinOp::Mul | BinOp::Div => (9, 10),
    }
}
