//! FerroDB — a small but real persistent SQL database engine.
//!
//! Layered architecture, bottom to top:
//!   pager   -> file-backed fixed-size pages with a cache and free list
//!   btree   -> persistent B+Tree (one per table) over the pager
//!   value   -> SQL values and row encoding stored in B+Tree cells
//!   catalog -> table schemas + roots, persisted in the catalog page
//!   sql     -> tokenizer, AST, and parser
//!   engine  -> the database: executes parsed statements against storage

pub mod btree;
pub mod catalog;
pub mod engine;
pub mod error;
pub mod pager;
pub mod sql;
pub mod value;

pub use engine::{Database, QueryResult};
pub use error::{DbError, Result};
pub use value::{ColumnType, Value};
