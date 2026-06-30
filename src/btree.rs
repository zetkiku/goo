//! A persistent B+Tree mapping `u64` keys to variable-length byte values.
//!
//! Each table in FerroDB is one B+Tree keyed by an auto-incrementing rowid.
//! Internal nodes route searches; leaf nodes hold the actual values and are
//! chained left-to-right via a `next` pointer for efficient full scans.
//!
//! Node page layout
//! ----------------
//! Leaf:     [0]=1 | [1..3]=num_cells(u16) | [3..7]=next_leaf(u32) | cells...
//!           cell = key(u64) | val_len(u32) | val_bytes
//! Internal: [0]=0 | [1..3]=num_keys(u16) | [3..7]=left_child(u32) |
//!           entries...  entry = key(u64) | child(u32)
//!
//! Deletion removes the cell from its leaf but does not merge/rebalance nodes
//! (a deliberately simple, fully-correct strategy: routing keys still point at
//! valid subtrees, and emptied leaves simply yield no results).

use crate::error::{DbError, Result};
use crate::pager::{
    read_u16, read_u32, read_u64, write_u16, write_u32, write_u64, PageId, Pager, PAGE_SIZE,
};

const NODE_LEAF: u8 = 1;
const NODE_INTERNAL: u8 = 0;

const LEAF_HEADER: usize = 7; // type(1) + num_cells(2) + next(4)
const INTERNAL_HEADER: usize = 7; // type(1) + num_keys(2) + left_child(4)

const LEAF_USABLE: usize = PAGE_SIZE - LEAF_HEADER;
const INTERNAL_USABLE: usize = PAGE_SIZE - INTERNAL_HEADER;

/// Largest value the tree will accept. Bounding a single cell to half the
/// usable leaf space guarantees that an overflowing leaf can always be split
/// into two leaves that each fit (see the proof in `split_leaf`).
pub const MAX_VALUE_LEN: usize = LEAF_USABLE / 2 - 12;

const INTERNAL_ENTRY: usize = 12; // key(8) + child(4)
const MAX_INTERNAL_KEYS: usize = INTERNAL_USABLE / INTERNAL_ENTRY;

// ---------------------------------------------------------------------------
// In-memory node representations (serialized to/from a page on demand)
// ---------------------------------------------------------------------------

struct LeafNode {
    next: PageId,
    cells: Vec<(u64, Vec<u8>)>, // sorted by key
}

impl LeafNode {
    fn read(buf: &[u8; PAGE_SIZE]) -> Result<LeafNode> {
        let n = read_u16(buf, 1) as usize;
        let next = read_u32(buf, 3);
        let mut cells = Vec::with_capacity(n);
        let mut pos = LEAF_HEADER;
        for _ in 0..n {
            let key = read_u64(buf, pos);
            pos += 8;
            let len = read_u32(buf, pos) as usize;
            pos += 4;
            let val = buf
                .get(pos..pos + len)
                .ok_or_else(|| DbError::Corrupt("leaf cell value out of range".into()))?
                .to_vec();
            pos += len;
            cells.push((key, val));
        }
        Ok(LeafNode { next, cells })
    }

    fn byte_size(&self) -> usize {
        LEAF_HEADER + self.cells.iter().map(|(_, v)| 12 + v.len()).sum::<usize>()
    }

    fn write(&self, buf: &mut [u8; PAGE_SIZE]) {
        buf.fill(0);
        buf[0] = NODE_LEAF;
        write_u16(buf, 1, self.cells.len() as u16);
        write_u32(buf, 3, self.next);
        let mut pos = LEAF_HEADER;
        for (key, val) in &self.cells {
            write_u64(buf, pos, *key);
            pos += 8;
            write_u32(buf, pos, val.len() as u32);
            pos += 4;
            buf[pos..pos + val.len()].copy_from_slice(val);
            pos += val.len();
        }
    }
}

struct InternalNode {
    left_child: PageId,
    entries: Vec<(u64, PageId)>, // (separator key, right child), sorted by key
}

impl InternalNode {
    fn read(buf: &[u8; PAGE_SIZE]) -> InternalNode {
        let n = read_u16(buf, 1) as usize;
        let left_child = read_u32(buf, 3);
        let mut entries = Vec::with_capacity(n);
        let mut pos = INTERNAL_HEADER;
        for _ in 0..n {
            let key = read_u64(buf, pos);
            pos += 8;
            let child = read_u32(buf, pos);
            pos += 4;
            entries.push((key, child));
        }
        InternalNode {
            left_child,
            entries,
        }
    }

    fn write(&self, buf: &mut [u8; PAGE_SIZE]) {
        buf.fill(0);
        buf[0] = NODE_INTERNAL;
        write_u16(buf, 1, self.entries.len() as u16);
        write_u32(buf, 3, self.left_child);
        let mut pos = INTERNAL_HEADER;
        for (key, child) in &self.entries {
            write_u64(buf, pos, *key);
            pos += 8;
            write_u32(buf, pos, *child);
            pos += 4;
        }
    }

    /// Choose the child page to descend into for `key`.
    fn child_for(&self, key: u64) -> PageId {
        let mut child = self.left_child;
        for (sep, c) in &self.entries {
            if key >= *sep {
                child = *c;
            } else {
                break;
            }
        }
        child
    }
}

fn node_type(pager: &mut Pager, id: PageId) -> Result<u8> {
    Ok(pager.get(id)?[0])
}

/// A handle to one table's B+Tree. The root page id may change when the root
/// splits, so callers must persist `root` afterwards (the catalog does this).
pub struct BTree {
    pub root: PageId,
}

/// Result of inserting into a subtree: `Some` means the child split and a new
/// separator/child pair must be inserted into the parent.
type SplitResult = Option<(u64, PageId)>;

impl BTree {
    /// Create an empty tree (a single empty leaf) and return its handle.
    pub fn create(pager: &mut Pager) -> Result<BTree> {
        let root = pager.allocate_page()?;
        let leaf = LeafNode {
            next: 0,
            cells: Vec::new(),
        };
        leaf.write(pager.get_mut(root)?);
        Ok(BTree { root })
    }

    pub fn open(root: PageId) -> BTree {
        BTree { root }
    }

    /// Insert or replace the value stored under `key`.
    pub fn insert(&mut self, pager: &mut Pager, key: u64, value: Vec<u8>) -> Result<()> {
        if value.len() > MAX_VALUE_LEN {
            return Err(DbError::PageFull(format!(
                "value of {} bytes exceeds maximum {}",
                value.len(),
                MAX_VALUE_LEN
            )));
        }
        if let Some((sep, new_page)) = self.insert_rec(pager, self.root, key, value)? {
            // Root split: build a new internal root above the old one.
            let new_root = pager.allocate_page()?;
            let node = InternalNode {
                left_child: self.root,
                entries: vec![(sep, new_page)],
            };
            node.write(pager.get_mut(new_root)?);
            self.root = new_root;
        }
        Ok(())
    }

    fn insert_rec(
        &mut self,
        pager: &mut Pager,
        page: PageId,
        key: u64,
        value: Vec<u8>,
    ) -> Result<SplitResult> {
        match node_type(pager, page)? {
            NODE_LEAF => {
                let mut leaf = LeafNode::read(pager.get(page)?)?;
                match leaf.cells.binary_search_by(|(k, _)| k.cmp(&key)) {
                    Ok(idx) => leaf.cells[idx].1 = value, // replace existing key
                    Err(idx) => leaf.cells.insert(idx, (key, value)),
                }
                if leaf.byte_size() <= PAGE_SIZE {
                    leaf.write(pager.get_mut(page)?);
                    Ok(None)
                } else {
                    self.split_leaf(pager, page, leaf)
                }
            }
            NODE_INTERNAL => {
                let node = InternalNode::read(pager.get(page)?);
                let child = node.child_for(key);
                let split = self.insert_rec(pager, child, key, value)?;
                if let Some((sep, new_child)) = split {
                    let mut node = InternalNode::read(pager.get(page)?);
                    let idx = node
                        .entries
                        .binary_search_by(|(k, _)| k.cmp(&sep))
                        .unwrap_or_else(|i| i);
                    node.entries.insert(idx, (sep, new_child));
                    if node.entries.len() <= MAX_INTERNAL_KEYS {
                        node.write(pager.get_mut(page)?);
                        Ok(None)
                    } else {
                        self.split_internal(pager, page, node)
                    }
                } else {
                    Ok(None)
                }
            }
            other => Err(DbError::Corrupt(format!("unknown node type {other}"))),
        }
    }

    /// Split an overflowing leaf into [page | new_page] and return the
    /// separator (smallest key of the right leaf) for the parent.
    fn split_leaf(&mut self, pager: &mut Pager, page: PageId, leaf: LeafNode) -> Result<SplitResult> {
        let total: usize = leaf.cells.iter().map(|(_, v)| 12 + v.len()).sum();
        // Greedy split: smallest prefix whose tail (right side) fits in a leaf.
        // Because every individual cell is <= LEAF_USABLE/2, both halves fit.
        let mut acc = 0usize;
        let mut split_at = 0usize;
        for (i, (_, v)) in leaf.cells.iter().enumerate() {
            acc += 12 + v.len();
            if total - acc <= LEAF_USABLE {
                split_at = i + 1;
                break;
            }
        }
        if split_at == 0 || split_at >= leaf.cells.len() {
            // Fallback to a balanced midpoint.
            split_at = leaf.cells.len() / 2;
        }

        let right_cells = leaf.cells[split_at..].to_vec();
        let left_cells = leaf.cells[..split_at].to_vec();
        let sep = right_cells[0].0;

        let new_page = pager.allocate_page()?;
        let right = LeafNode {
            next: leaf.next,
            cells: right_cells,
        };
        right.write(pager.get_mut(new_page)?);

        let left = LeafNode {
            next: new_page,
            cells: left_cells,
        };
        left.write(pager.get_mut(page)?);

        Ok(Some((sep, new_page)))
    }

    /// Split an overflowing internal node, promoting the middle separator.
    fn split_internal(
        &mut self,
        pager: &mut Pager,
        page: PageId,
        node: InternalNode,
    ) -> Result<SplitResult> {
        let mid = node.entries.len() / 2;
        let promote_key = node.entries[mid].0;
        // entries[mid].child becomes the left_child of the new right node.
        let right_left_child = node.entries[mid].1;
        let right_entries = node.entries[mid + 1..].to_vec();
        let left_entries = node.entries[..mid].to_vec();

        let new_page = pager.allocate_page()?;
        let right = InternalNode {
            left_child: right_left_child,
            entries: right_entries,
        };
        right.write(pager.get_mut(new_page)?);

        let left = InternalNode {
            left_child: node.left_child,
            entries: left_entries,
        };
        left.write(pager.get_mut(page)?);

        Ok(Some((promote_key, new_page)))
    }

    /// Look up the value stored under `key`.
    pub fn get(&self, pager: &mut Pager, key: u64) -> Result<Option<Vec<u8>>> {
        let mut page = self.root;
        loop {
            match node_type(pager, page)? {
                NODE_LEAF => {
                    let leaf = LeafNode::read(pager.get(page)?)?;
                    return Ok(leaf
                        .cells
                        .binary_search_by(|(k, _)| k.cmp(&key))
                        .ok()
                        .map(|idx| leaf.cells[idx].1.clone()));
                }
                NODE_INTERNAL => {
                    let node = InternalNode::read(pager.get(page)?);
                    page = node.child_for(key);
                }
                other => return Err(DbError::Corrupt(format!("unknown node type {other}"))),
            }
        }
    }

    /// Remove `key`. Returns true if a value was actually removed.
    pub fn delete(&mut self, pager: &mut Pager, key: u64) -> Result<bool> {
        let mut page = self.root;
        loop {
            match node_type(pager, page)? {
                NODE_LEAF => {
                    let mut leaf = LeafNode::read(pager.get(page)?)?;
                    if let Ok(idx) = leaf.cells.binary_search_by(|(k, _)| k.cmp(&key)) {
                        leaf.cells.remove(idx);
                        leaf.write(pager.get_mut(page)?);
                        return Ok(true);
                    }
                    return Ok(false);
                }
                NODE_INTERNAL => {
                    let node = InternalNode::read(pager.get(page)?);
                    page = node.child_for(key);
                }
                other => return Err(DbError::Corrupt(format!("unknown node type {other}"))),
            }
        }
    }

    /// Return every (key, value) pair in ascending key order.
    pub fn scan(&self, pager: &mut Pager) -> Result<Vec<(u64, Vec<u8>)>> {
        // Descend to the leftmost leaf.
        let mut page = self.root;
        loop {
            match node_type(pager, page)? {
                NODE_LEAF => break,
                NODE_INTERNAL => {
                    let node = InternalNode::read(pager.get(page)?);
                    page = node.left_child;
                }
                other => return Err(DbError::Corrupt(format!("unknown node type {other}"))),
            }
        }
        // Walk the leaf chain.
        let mut out = Vec::new();
        loop {
            let leaf = LeafNode::read(pager.get(page)?)?;
            out.extend(leaf.cells.iter().cloned());
            if leaf.next == 0 {
                break;
            }
            page = leaf.next;
        }
        Ok(out)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::pager::Pager;
    use std::sync::atomic::{AtomicU64, Ordering};

    static C: AtomicU64 = AtomicU64::new(0);

    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            let n = C.fetch_add(1, Ordering::SeqCst);
            let mut p = std::env::temp_dir();
            p.push(format!("ferro-btree-{}-{}.db", std::process::id(), n));
            let _ = std::fs::remove_file(&p);
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn insert_get_delete() {
        let tmp = Tmp::new();
        let mut pager = Pager::open(&tmp.0).unwrap();
        let mut tree = BTree::create(&mut pager).unwrap();

        tree.insert(&mut pager, 5, b"five".to_vec()).unwrap();
        tree.insert(&mut pager, 1, b"one".to_vec()).unwrap();
        tree.insert(&mut pager, 3, b"three".to_vec()).unwrap();

        assert_eq!(tree.get(&mut pager, 3).unwrap(), Some(b"three".to_vec()));
        assert_eq!(tree.get(&mut pager, 99).unwrap(), None);

        // Replace existing key.
        tree.insert(&mut pager, 3, b"THREE".to_vec()).unwrap();
        assert_eq!(tree.get(&mut pager, 3).unwrap(), Some(b"THREE".to_vec()));

        assert!(tree.delete(&mut pager, 3).unwrap());
        assert_eq!(tree.get(&mut pager, 3).unwrap(), None);
        assert!(!tree.delete(&mut pager, 3).unwrap());
    }

    #[test]
    fn scan_is_sorted_after_splits() {
        let tmp = Tmp::new();
        let mut pager = Pager::open(&tmp.0).unwrap();
        let mut tree = BTree::create(&mut pager).unwrap();

        // Insert keys in a shuffled order; large values to force many splits.
        let mut keys: Vec<u64> = (0..3000).collect();
        // Simple deterministic shuffle.
        keys.sort_by_key(|k| (k.wrapping_mul(2654435761)) & 0xffff);
        for k in &keys {
            tree.insert(&mut pager, *k, format!("payload-{k}").into_bytes())
                .unwrap();
        }

        let scanned = tree.scan(&mut pager).unwrap();
        assert_eq!(scanned.len(), 3000);
        // Keys must come out in ascending order.
        for w in scanned.windows(2) {
            assert!(w[0].0 < w[1].0, "scan not sorted at {:?}", w[0].0);
        }
        // Values must match their keys.
        for (k, v) in &scanned {
            assert_eq!(v, &format!("payload-{k}").into_bytes());
        }
    }

    #[test]
    fn persists_across_reopen() {
        let tmp = Tmp::new();
        let root;
        {
            let mut pager = Pager::open(&tmp.0).unwrap();
            let mut tree = BTree::create(&mut pager).unwrap();
            for i in 0..500u64 {
                tree.insert(&mut pager, i, format!("v{i}").into_bytes())
                    .unwrap();
            }
            root = tree.root;
            pager.flush().unwrap();
        }
        {
            let mut pager = Pager::open(&tmp.0).unwrap();
            let tree = BTree::open(root);
            assert_eq!(tree.get(&mut pager, 250).unwrap(), Some(b"v250".to_vec()));
            assert_eq!(tree.scan(&mut pager).unwrap().len(), 500);
        }
    }
}
