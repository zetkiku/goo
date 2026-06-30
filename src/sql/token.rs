//! Hand-written SQL tokenizer (lexer).

use crate::error::{DbError, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Integer(i64),
    Text(String),
    Identifier(String),
    Keyword(Keyword),

    LParen,
    RParen,
    Comma,
    Semicolon,
    Star,
    Dot,

    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Slash,

    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Select,
    From,
    Where,
    Insert,
    Into,
    Values,
    Create,
    Table,
    Update,
    Set,
    Delete,
    Drop,
    Begin,
    Commit,
    Rollback,
    Transaction,
    And,
    Or,
    Not,
    Null,
    Limit,
    OrderBy, // synthetic: handled as ORDER + BY below
    Order,
    By,
    Asc,
    Desc,
    Integer,
    Text,
    True,
    False,
    Join,
    Inner,
    On,
    As,
    Group,
}

fn keyword_from(s: &str) -> Option<Keyword> {
    let up = s.to_ascii_uppercase();
    Some(match up.as_str() {
        "SELECT" => Keyword::Select,
        "FROM" => Keyword::From,
        "WHERE" => Keyword::Where,
        "INSERT" => Keyword::Insert,
        "INTO" => Keyword::Into,
        "VALUES" => Keyword::Values,
        "CREATE" => Keyword::Create,
        "TABLE" => Keyword::Table,
        "UPDATE" => Keyword::Update,
        "SET" => Keyword::Set,
        "DELETE" => Keyword::Delete,
        "DROP" => Keyword::Drop,
        "BEGIN" | "START" => Keyword::Begin,
        "COMMIT" => Keyword::Commit,
        "ROLLBACK" => Keyword::Rollback,
        "TRANSACTION" => Keyword::Transaction,
        "AND" => Keyword::And,
        "OR" => Keyword::Or,
        "NOT" => Keyword::Not,
        "NULL" => Keyword::Null,
        "LIMIT" => Keyword::Limit,
        "ORDER" => Keyword::Order,
        "BY" => Keyword::By,
        "ASC" => Keyword::Asc,
        "DESC" => Keyword::Desc,
        "INTEGER" | "INT" => Keyword::Integer,
        "TEXT" | "VARCHAR" | "STRING" => Keyword::Text,
        "TRUE" => Keyword::True,
        "FALSE" => Keyword::False,
        "JOIN" => Keyword::Join,
        "INNER" => Keyword::Inner,
        "ON" => Keyword::On,
        "AS" => Keyword::As,
        "GROUP" => Keyword::Group,
        _ => return None,
    })
}

pub struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Lexer<'a> {
        Lexer {
            chars: src.chars().peekable(),
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut out = Vec::new();
        loop {
            let tok = self.next_token()?;
            let is_eof = tok == Token::Eof;
            out.push(tok);
            if is_eof {
                break;
            }
        }
        Ok(out)
    }

    fn next_token(&mut self) -> Result<Token> {
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() {
                self.chars.next();
            } else {
                break;
            }
        }

        let c = match self.chars.peek() {
            Some(&c) => c,
            None => return Ok(Token::Eof),
        };

        match c {
            '(' => {
                self.chars.next();
                Ok(Token::LParen)
            }
            ')' => {
                self.chars.next();
                Ok(Token::RParen)
            }
            ',' => {
                self.chars.next();
                Ok(Token::Comma)
            }
            ';' => {
                self.chars.next();
                Ok(Token::Semicolon)
            }
            '*' => {
                self.chars.next();
                Ok(Token::Star)
            }
            '.' => {
                self.chars.next();
                Ok(Token::Dot)
            }
            '+' => {
                self.chars.next();
                Ok(Token::Plus)
            }
            '-' => {
                self.chars.next();
                Ok(Token::Minus)
            }
            '/' => {
                self.chars.next();
                Ok(Token::Slash)
            }
            '=' => {
                self.chars.next();
                Ok(Token::Eq)
            }
            '!' => {
                self.chars.next();
                if self.chars.peek() == Some(&'=') {
                    self.chars.next();
                    Ok(Token::Ne)
                } else {
                    Err(DbError::Parse("unexpected '!' (did you mean '!='?)".into()))
                }
            }
            '<' => {
                self.chars.next();
                match self.chars.peek() {
                    Some('=') => {
                        self.chars.next();
                        Ok(Token::Le)
                    }
                    Some('>') => {
                        self.chars.next();
                        Ok(Token::Ne)
                    }
                    _ => Ok(Token::Lt),
                }
            }
            '>' => {
                self.chars.next();
                if self.chars.peek() == Some(&'=') {
                    self.chars.next();
                    Ok(Token::Ge)
                } else {
                    Ok(Token::Gt)
                }
            }
            '\'' => self.read_string(),
            c if c.is_ascii_digit() => self.read_number(),
            c if c.is_alphabetic() || c == '_' => Ok(self.read_word()),
            other => Err(DbError::Parse(format!("unexpected character '{other}'"))),
        }
    }

    fn read_string(&mut self) -> Result<Token> {
        self.chars.next(); // opening quote
        let mut s = String::new();
        loop {
            match self.chars.next() {
                Some('\'') => {
                    // Two single quotes in a row => an escaped quote.
                    if self.chars.peek() == Some(&'\'') {
                        self.chars.next();
                        s.push('\'');
                    } else {
                        return Ok(Token::Text(s));
                    }
                }
                Some(c) => s.push(c),
                None => return Err(DbError::Parse("unterminated string literal".into())),
            }
        }
    }

    fn read_number(&mut self) -> Result<Token> {
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() {
                s.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        s.parse::<i64>()
            .map(Token::Integer)
            .map_err(|_| DbError::Parse(format!("invalid integer literal '{s}'")))
    }

    fn read_word(&mut self) -> Token {
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_alphanumeric() || c == '_' {
                s.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        match keyword_from(&s) {
            Some(kw) => Token::Keyword(kw),
            None => Token::Identifier(s),
        }
    }
}
