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
            other => Err(DbError::Parse(format!("expected identifier, found {other:?}"))),
        }
    }

    // --- statements --------------------------------------------------------

    fn parse_statement(&mut self) -> Result<Statement> {
        match self.peek() {
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
        let projection = if self.peek() == &Token::Star {
            self.advance();
            Projection::All
        } else {
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_identifier()?);
                if self.peek() == &Token::Comma {
                    self.advance();
                } else {
                    break;
                }
            }
            Projection::Columns(cols)
        };
        self.expect_keyword(Keyword::From)?;
        let table = self.expect_identifier()?;

        let filter = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        let order_by = if self.eat_keyword(Keyword::Order) {
            self.expect_keyword(Keyword::By)?;
            let col = self.expect_identifier()?;
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

        Ok(Statement::Select {
            table,
            projection,
            filter,
            order_by,
            limit,
        })
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
                Ok(Expr::Column(name))
            }
            other => Err(DbError::Parse(format!(
                "unexpected token in expression: {other:?}"
            ))),
        }
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
