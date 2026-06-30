//! The pager: the only component that touches the database file.
//!
//! The file is an array of fixed-size 4 KiB pages addressed by a `u32` id.
//!
//!   page 0  -> meta page (magic, version, page count, free-list head, catalog page)
//!   page 1+ -> catalog page, B+Tree nodes, and recycled free pages
//!
//! Freed pages are threaded onto a singly-linked free list whose head is stored
//! in the meta page; each free page stores the id of the next free page in its
//! first four bytes. Pages are cached in memory and written back on `flush`.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{DbError, Result};

pub const PAGE_SIZE: usize = 4096;
pub type PageId = u32;

const MAGIC: &[u8; 4] = b"FRDB";
const VERSION: u32 = 1;

/// Meta page byte offsets.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_PAGE_COUNT: usize = 8;
const OFF_FREELIST: usize = 12;
const OFF_CATALOG: usize = 16;

pub struct Pager {
    file: File,
    cache: HashMap<PageId, Box<[u8; PAGE_SIZE]>>,
    dirty: HashSet<PageId>,
    page_count: u32,
    freelist_head: PageId,
    catalog_page: PageId,
}

impl Pager {
    /// Open an existing database or create a fresh, fully-initialized one.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Pager> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let len = file.seek(SeekFrom::End(0))?;

        if len == 0 {
            // Brand new database: write meta page (0) and an empty catalog page (1).
            let mut pager = Pager {
                file,
                cache: HashMap::new(),
                dirty: HashSet::new(),
                page_count: 1, // meta page exists
                freelist_head: 0,
                catalog_page: 0,
            };
            let catalog = pager.allocate_page()?;
            pager.catalog_page = catalog;
            // Zero the catalog page so the catalog deserializes as "empty".
            {
                let p = pager.get_mut(catalog)?;
                p.fill(0);
            }
            pager.write_meta()?;
            pager.flush()?;
            Ok(pager)
        } else {
            let mut meta = [0u8; PAGE_SIZE];
            file.seek(SeekFrom::Start(0))?;
            file.read_exact(&mut meta)?;
            if &meta[OFF_MAGIC..OFF_MAGIC + 4] != MAGIC {
                return Err(DbError::Corrupt("bad magic header".into()));
            }
            let version = read_u32(&meta, OFF_VERSION);
            if version != VERSION {
                return Err(DbError::Corrupt(format!("unsupported version {version}")));
            }
            let page_count = read_u32(&meta, OFF_PAGE_COUNT);
            let freelist_head = read_u32(&meta, OFF_FREELIST);
            let catalog_page = read_u32(&meta, OFF_CATALOG);
            Ok(Pager {
                file,
                cache: HashMap::new(),
                dirty: HashSet::new(),
                page_count,
                freelist_head,
                catalog_page,
            })
        }
    }

    pub fn catalog_page(&self) -> PageId {
        self.catalog_page
    }

    /// Load a page into cache (or read from disk) and return a shared reference.
    pub fn get(&mut self, id: PageId) -> Result<&[u8; PAGE_SIZE]> {
        self.ensure_cached(id)?;
        Ok(self.cache.get(&id).unwrap())
    }

    /// Load a page and return a mutable reference, marking it dirty.
    pub fn get_mut(&mut self, id: PageId) -> Result<&mut [u8; PAGE_SIZE]> {
        self.ensure_cached(id)?;
        self.dirty.insert(id);
        Ok(self.cache.get_mut(&id).unwrap())
    }

    fn ensure_cached(&mut self, id: PageId) -> Result<()> {
        if self.cache.contains_key(&id) {
            return Ok(());
        }
        if id >= self.page_count {
            return Err(DbError::Corrupt(format!(
                "page {id} out of range (page_count={})",
                self.page_count
            )));
        }
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        self.file.seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))?;
        self.file.read_exact(buf.as_mut_slice())?;
        self.cache.insert(id, buf);
        Ok(())
    }

    /// Allocate a page, reusing the free list when possible.
    pub fn allocate_page(&mut self) -> Result<PageId> {
        if self.freelist_head != 0 {
            let id = self.freelist_head;
            let next = read_u32(self.get(id)?, 0);
            self.freelist_head = next;
            let p = self.get_mut(id)?;
            p.fill(0);
            return Ok(id);
        }
        let id = self.page_count;
        self.page_count += 1;
        // Insert a zeroed page directly into the cache (no disk read needed).
        self.cache.insert(id, Box::new([0u8; PAGE_SIZE]));
        self.dirty.insert(id);
        Ok(id)
    }

    /// Return a page to the free list so a later allocation can reuse it.
    pub fn free_page(&mut self, id: PageId) -> Result<()> {
        let old_head = self.freelist_head;
        let p = self.get_mut(id)?;
        p.fill(0);
        write_u32(p, 0, old_head);
        self.freelist_head = id;
        Ok(())
    }

    fn write_meta(&mut self) -> Result<()> {
        let page_count = self.page_count;
        let freelist = self.freelist_head;
        let catalog = self.catalog_page;
        // Meta page is page 0; make sure it exists in cache.
        self.cache
            .entry(0)
            .or_insert_with(|| Box::new([0u8; PAGE_SIZE]));
        let p: &mut [u8] = self.cache.get_mut(&0).unwrap().as_mut_slice();
        p[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(MAGIC);
        write_u32(p, OFF_VERSION, VERSION);
        write_u32(p, OFF_PAGE_COUNT, page_count);
        write_u32(p, OFF_FREELIST, freelist);
        write_u32(p, OFF_CATALOG, catalog);
        self.dirty.insert(0);
        Ok(())
    }

    /// Persist every dirty page (and the meta page) to disk.
    pub fn flush(&mut self) -> Result<()> {
        self.write_meta()?;
        let mut ids: Vec<PageId> = self.dirty.iter().copied().collect();
        ids.sort_unstable();
        for id in ids {
            if let Some(buf) = self.cache.get(&id) {
                self.file
                    .seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))?;
                self.file.write_all(buf.as_slice())?;
            }
        }
        self.file.flush()?;
        self.file.sync_all()?;
        self.dirty.clear();
        Ok(())
    }
}

pub fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

pub fn write_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

pub fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn read_u64(buf: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(a)
}

pub fn write_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
