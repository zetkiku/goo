//! The catalog: the database's metadata about tables.
//!
//! It is serialized into the single catalog page (see `Pager::catalog_page`).
//! Format (little-endian):
//!   num_tables(u16)
//!   per table: name_len(u16) name | root_page(u32) | next_rowid(u64) |
//!              num_cols(u16) | per col: name_len(u16) name | type(u8)

use std::collections::BTreeMap;

use crate::error::{DbError, Result};
use crate::pager::{read_u16, read_u32, read_u64, Pager, PAGE_SIZE};
use crate::value::ColumnType;

#[derive(Debug, Clone)]
pub struct TableDef {
    pub name: String,
    pub root: u32,
    pub next_rowid: u64,
    pub columns: Vec<(String, ColumnType)>,
}

impl TableDef {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(name))
    }
}

pub struct Catalog {
    // BTreeMap keeps a stable, sorted iteration order.
    tables: BTreeMap<String, TableDef>,
}

impl Catalog {
    pub fn load(pager: &mut Pager) -> Result<Catalog> {
        let page_id = pager.catalog_page();
        let buf = pager.get(page_id)?;
        let mut tables = BTreeMap::new();
        let num_tables = read_u16(buf, 0) as usize;
        let mut pos = 2usize;

        let read_str = |buf: &[u8], pos: &mut usize| -> Result<String> {
            let len = read_u16(buf, *pos) as usize;
            *pos += 2;
            let s = buf
                .get(*pos..*pos + len)
                .ok_or_else(|| DbError::Corrupt("catalog string out of range".into()))?;
            let out = String::from_utf8(s.to_vec())
                .map_err(|_| DbError::Corrupt("catalog string not utf-8".into()))?;
            *pos += len;
            Ok(out)
        };

        for _ in 0..num_tables {
            let name = read_str(buf, &mut pos)?;
            let root = read_u32(buf, pos);
            pos += 4;
            let next_rowid = read_u64(buf, pos);
            pos += 8;
            let ncols = read_u16(buf, pos) as usize;
            pos += 2;
            let mut columns = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                let cname = read_str(buf, &mut pos)?;
                let ty = match buf[pos] {
                    0 => ColumnType::Integer,
                    1 => ColumnType::Text,
                    other => return Err(DbError::Corrupt(format!("unknown column type {other}"))),
                };
                pos += 1;
                columns.push((cname, ty));
            }
            tables.insert(
                name.to_ascii_lowercase(),
                TableDef {
                    name,
                    root,
                    next_rowid,
                    columns,
                },
            );
        }
        Ok(Catalog { tables })
    }

    pub fn save(&self, pager: &mut Pager) -> Result<()> {
        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(&(self.tables.len() as u16).to_le_bytes());

        let push_str = |data: &mut Vec<u8>, s: &str| {
            data.extend_from_slice(&(s.len() as u16).to_le_bytes());
            data.extend_from_slice(s.as_bytes());
        };

        for t in self.tables.values() {
            push_str(&mut data, &t.name);
            data.extend_from_slice(&t.root.to_le_bytes());
            data.extend_from_slice(&t.next_rowid.to_le_bytes());
            data.extend_from_slice(&(t.columns.len() as u16).to_le_bytes());
            for (cname, ty) in &t.columns {
                push_str(&mut data, cname);
                data.push(match ty {
                    ColumnType::Integer => 0,
                    ColumnType::Text => 1,
                });
            }
        }

        if data.len() > PAGE_SIZE {
            return Err(DbError::PageFull(
                "catalog exceeds one page; too many tables/columns".into(),
            ));
        }

        let page_id = pager.catalog_page();
        let buf = pager.get_mut(page_id)?;
        buf.fill(0);
        buf[..data.len()].copy_from_slice(&data);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&TableDef> {
        self.tables.get(&name.to_ascii_lowercase())
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut TableDef> {
        self.tables.get_mut(&name.to_ascii_lowercase())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tables.contains_key(&name.to_ascii_lowercase())
    }

    pub fn insert(&mut self, def: TableDef) {
        self.tables.insert(def.name.to_ascii_lowercase(), def);
    }

    pub fn remove(&mut self, name: &str) -> Option<TableDef> {
        self.tables.remove(&name.to_ascii_lowercase())
    }

    pub fn table_names(&self) -> Vec<String> {
        self.tables.values().map(|t| t.name.clone()).collect()
    }
}
