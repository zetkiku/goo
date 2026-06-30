//! SQL values, column types, and the binary encoding used to store rows
//! inside B+Tree leaf cells.
//!
//! Row encoding (self-describing, little-endian):
//!   for each value:
//!     1 byte tag: 0 = NULL, 1 = INTEGER, 2 = TEXT
//!     INTEGER -> 8 bytes i64
//!     TEXT    -> 4 bytes u32 length, then that many UTF-8 bytes

use crate::error::{DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    Text,
}

impl ColumnType {
    pub fn name(&self) -> &'static str {
        match self {
            ColumnType::Integer => "INTEGER",
            ColumnType::Text => "TEXT",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Text(String),
}

impl Value {
    /// SQL truthiness, used for WHERE clause evaluation.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Integer(n) => *n != 0,
            Value::Text(s) => !s.is_empty(),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Integer(_) => "INTEGER",
            Value::Text(_) => "TEXT",
        }
    }

    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Integer(n) => n.to_string(),
            Value::Text(s) => s.clone(),
        }
    }
}

pub type Row = Vec<Value>;

/// Serialize a row into a compact byte buffer for storage.
pub fn encode_row(row: &Row) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 * row.len());
    for v in row {
        match v {
            Value::Null => buf.push(0),
            Value::Integer(n) => {
                buf.push(1);
                buf.extend_from_slice(&n.to_le_bytes());
            }
            Value::Text(s) => {
                buf.push(2);
                let bytes = s.as_bytes();
                buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(bytes);
            }
        }
    }
    buf
}

/// Decode a row of `ncols` values from a byte buffer.
pub fn decode_row(buf: &[u8], ncols: usize) -> Result<Row> {
    let mut row = Vec::with_capacity(ncols);
    let mut pos = 0usize;
    for _ in 0..ncols {
        let tag = *buf
            .get(pos)
            .ok_or_else(|| DbError::Corrupt("row truncated at tag".into()))?;
        pos += 1;
        match tag {
            0 => row.push(Value::Null),
            1 => {
                let end = pos + 8;
                let slice = buf
                    .get(pos..end)
                    .ok_or_else(|| DbError::Corrupt("row truncated at int".into()))?;
                let mut arr = [0u8; 8];
                arr.copy_from_slice(slice);
                row.push(Value::Integer(i64::from_le_bytes(arr)));
                pos = end;
            }
            2 => {
                let len_end = pos + 4;
                let len_slice = buf
                    .get(pos..len_end)
                    .ok_or_else(|| DbError::Corrupt("row truncated at text len".into()))?;
                let mut larr = [0u8; 4];
                larr.copy_from_slice(len_slice);
                let len = u32::from_le_bytes(larr) as usize;
                pos = len_end;
                let str_end = pos + len;
                let sslice = buf
                    .get(pos..str_end)
                    .ok_or_else(|| DbError::Corrupt("row truncated at text body".into()))?;
                let s = String::from_utf8(sslice.to_vec())
                    .map_err(|_| DbError::Corrupt("invalid utf-8 in text".into()))?;
                row.push(Value::Text(s));
                pos = str_end;
            }
            other => {
                return Err(DbError::Corrupt(format!("unknown value tag {other}")));
            }
        }
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_encode_decode_roundtrip() {
        let row = vec![
            Value::Integer(-42),
            Value::Text("hello, world".into()),
            Value::Null,
            Value::Integer(i64::MAX),
        ];
        let bytes = encode_row(&row);
        let decoded = decode_row(&bytes, row.len()).unwrap();
        assert_eq!(row, decoded);
    }

    #[test]
    fn empty_text_roundtrip() {
        let row = vec![Value::Text(String::new())];
        let bytes = encode_row(&row);
        assert_eq!(decode_row(&bytes, 1).unwrap(), row);
    }

    #[test]
    fn truthiness() {
        assert!(!Value::Null.is_truthy());
        assert!(!Value::Integer(0).is_truthy());
        assert!(Value::Integer(1).is_truthy());
        assert!(!Value::Text(String::new()).is_truthy());
        assert!(Value::Text("x".into()).is_truthy());
    }
}
