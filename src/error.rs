//! Unified error type for every layer of FerroDB.

use std::fmt;

#[derive(Debug)]
pub enum DbError {
    /// Low-level I/O failure from the pager.
    Io(std::io::Error),
    /// The on-disk file is not a FerroDB database (bad magic / version).
    Corrupt(String),
    /// A page or node could not hold the data it was asked to store.
    PageFull(String),
    /// Lexer / parser failure with a human-readable message.
    Parse(String),
    /// Semantic / runtime failure during query execution.
    Exec(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Io(e) => write!(f, "io error: {e}"),
            DbError::Corrupt(m) => write!(f, "corrupt database: {m}"),
            DbError::PageFull(m) => write!(f, "page full: {m}"),
            DbError::Parse(m) => write!(f, "parse error: {m}"),
            DbError::Exec(m) => write!(f, "execution error: {m}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        DbError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, DbError>;
