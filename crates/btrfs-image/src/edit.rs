//! Copy-on-read editing of btrfs trees for the image: nodes are read at the generation being
//! imaged, changed in memory, and the changed copies (checksums recomputed) replace the
//! originals in what is sent. The device is never written.
//!
//! Only edits that fit in the existing nodes are supported: deleting and updating items,
//! and inserting into a leaf with enough free space. Anything else (splitting or merging
//! nodes) is reported as [`Error::Unsupported`].

use crate::disk::*;
use crate::{Error, Filesystem};
use std::collections::HashMap;

pub struct Editor<'a> {
    fs: &'a Filesystem,
    changed: HashMap<u64, Vec<u8>>,
    nodesize: usize,
}

/// Path from a tree root to a leaf: (node, slot) per level, root first.
type Path = Vec<(u64, usize)>;

fn nritems(n: &[u8]) -> usize {
    u32_at(n, 96) as usize
}

fn set_nritems(n: &mut [u8], v: usize) {
    n[96..100].copy_from_slice(&(v as u32).to_le_bytes());
}

fn level(n: &[u8]) -> u8 {
    n[100]
}

fn item_hdr(i: usize) -> usize {
    HEADER_SIZE + i * ITEM_SIZE
}

fn item_key(n: &[u8], i: usize) -> Key {
    Key::parse(&n[item_hdr(i)..])
}

fn item_off(n: &[u8], i: usize) -> usize {
    u32_at(n, item_hdr(i) + 17) as usize
}

fn item_size(n: &[u8], i: usize) -> usize {
    u32_at(n, item_hdr(i) + 21) as usize
}

fn set_item(n: &mut [u8], i: usize, key: Key, off: usize, size: usize) {
    let h = item_hdr(i);
    write_key(&mut n[h..h + 17], key);
    n[h + 17..h + 21].copy_from_slice(&(off as u32).to_le_bytes());
    n[h + 21..h + 25].copy_from_slice(&(size as u32).to_le_bytes());
}

fn ptr_key(n: &[u8], i: usize) -> Key {
    Key::parse(&n[HEADER_SIZE + i * KEY_PTR_SIZE..])
}

fn ptr_child(n: &[u8], i: usize) -> u64 {
    u64_at(n, HEADER_SIZE + i * KEY_PTR_SIZE + 17)
}

fn write_key(b: &mut [u8], k: Key) {
    b[0..8].copy_from_slice(&k.objectid.to_le_bytes());
    b[8] = k.kind;
    b[9..17].copy_from_slice(&k.offset.to_le_bytes());
}

impl<'a> Editor<'a> {
    pub fn new(fs: &'a Filesystem) -> Editor<'a> {
        Editor { fs, changed: HashMap::new(), nodesize: fs.sb.nodesize as usize }
    }

    fn node(&self, logical: u64) -> Result<Vec<u8>, Error> {
        match self.changed.get(&logical) {
            Some(n) => Ok(n.clone()),
            None => Ok(self.fs.read_node(logical, 0)?.data),
        }
    }

    /// Descends from `root` towards `key`. At the leaf the slot is the first item `>= key`;
    /// `true` if that item has exactly `key`.
    fn search(&self, root: u64, key: Key) -> Result<(Path, bool), Error> {
        let mut path = Vec::new();
        let mut logical = root;
        loop {
            let n = self.node(logical)?;
            let count = nritems(&n);
            if level(&n) == 0 {
                let slot = (0..count).find(|&i| item_key(&n, i) >= key).unwrap_or(count);
                let found = slot < count && item_key(&n, slot) == key;
                path.push((logical, slot));
                return Ok((path, found));
            }
            // Last pointer whose key is <= key (or the first one).
            let slot = (0..count).rev().find(|&i| ptr_key(&n, i) <= key).unwrap_or(0);
            path.push((logical, slot));
            logical = ptr_child(&n, slot);
        }
    }

    /// First item with this objectid and type (any offset), if any.
    pub fn find_first(&self, root: u64, objectid: u64, kind: u8) -> Result<Option<(Key, Vec<u8>)>, Error> {
        let (path, _) = self.search(root, Key { objectid, kind, offset: 0 })?;
        let (leaf, slot) = *path.last().unwrap();
        let n = self.node(leaf)?;
        let mut slot = slot;
        let mut leaf_data = n;
        if slot >= nritems(&leaf_data) {
            // The match, if any, is the first item of the next leaf.
            match self.next_leaf(&path)? {
                Some(next) => {
                    leaf_data = self.node(next)?;
                    slot = 0;
                }
                None => return Ok(None),
            }
        }
        if slot >= nritems(&leaf_data) {
            return Ok(None);
        }
        let k = item_key(&leaf_data, slot);
        if k.objectid != objectid || k.kind != kind {
            return Ok(None);
        }
        let off = HEADER_SIZE + item_off(&leaf_data, slot);
        Ok(Some((k, leaf_data[off..off + item_size(&leaf_data, slot)].to_vec())))
    }

    fn next_leaf(&self, path: &Path) -> Result<Option<u64>, Error> {
        for lvl in (0..path.len() - 1).rev() {
            let (logical, slot) = path[lvl];
            let n = self.node(logical)?;
            if slot + 1 < nritems(&n) {
                let mut child = ptr_child(&n, slot + 1);
                loop {
                    let c = self.node(child)?;
                    if level(&c) == 0 {
                        return Ok(Some(child));
                    }
                    child = ptr_child(&c, 0);
                }
            }
        }
        Ok(None)
    }

    pub fn get(&self, root: u64, key: Key) -> Result<Option<Vec<u8>>, Error> {
        let (path, found) = self.search(root, key)?;
        if !found {
            return Ok(None);
        }
        let (leaf, slot) = *path.last().unwrap();
        let n = self.node(leaf)?;
        let off = HEADER_SIZE + item_off(&n, slot);
        Ok(Some(n[off..off + item_size(&n, slot)].to_vec()))
    }

    /// After the first key of a node changed, updates the key pointers above it.
    fn fix_keys(&mut self, path: &Path, mut lvl: usize, key: Key) -> Result<(), Error> {
        while lvl > 0 {
            let (child_slot_parent, slot) = path[lvl - 1];
            let mut p = self.node(child_slot_parent)?;
            let o = HEADER_SIZE + slot * KEY_PTR_SIZE;
            write_key(&mut p[o..o + 17], key);
            self.changed.insert(child_slot_parent, p);
            if slot != 0 {
                break;
            }
            lvl -= 1;
        }
        Ok(())
    }

    /// Items of a leaf: (key, data).
    fn leaf_items(n: &[u8]) -> Vec<(Key, Vec<u8>)> {
        (0..nritems(n))
            .map(|i| {
                let off = HEADER_SIZE + item_off(n, i);
                (item_key(n, i), n[off..off + item_size(n, i)].to_vec())
            })
            .collect()
    }

    /// Rewrites leaf `n` with `items` (sorted), packing the data at the end as btrfs does.
    fn pack_leaf(&self, n: &mut [u8], items: &[(Key, Vec<u8>)]) -> Result<(), Error> {
        let need: usize = items.iter().map(|(_, d)| d.len() + ITEM_SIZE).sum();
        if items.is_empty() || need > self.nodesize - HEADER_SIZE {
            return Err(Error::Unsupported("the edited leaf would be empty or overflow".into()));
        }
        n[HEADER_SIZE..].fill(0);
        let mut off = self.nodesize - HEADER_SIZE;
        for (i, (k, d)) in items.iter().enumerate() {
            off -= d.len();
            set_item(n, i, *k, off, d.len());
            n[HEADER_SIZE + off..HEADER_SIZE + off + d.len()].copy_from_slice(d);
        }
        set_nritems(n, items.len());
        Ok(())
    }

    /// Applies `f` to the items of the leaf where `key` is or would be, then rewrites it.
    fn edit_leaf(&mut self, root: u64, key: Key, f: impl FnOnce(&mut Vec<(Key, Vec<u8>)>, usize, bool) -> Result<(), Error>) -> Result<(), Error> {
        let (path, found) = self.search(root, key)?;
        let (leaf, slot) = *path.last().unwrap();
        let mut n = self.node(leaf)?;
        let mut items = Self::leaf_items(&n);
        let first_before = items.first().map(|i| i.0);
        f(&mut items, slot, found)?;
        self.pack_leaf(&mut n, &items)?;
        self.changed.insert(leaf, n);
        let first = items[0].0;
        if Some(first) != first_before {
            self.fix_keys(&path, path.len() - 1, first)?;
        }
        Ok(())
    }

    /// Changes an item in place (same size).
    pub fn update(&mut self, root: u64, key: Key, f: impl FnOnce(&mut [u8])) -> Result<(), Error> {
        self.edit_leaf(root, key, |items, slot, found| {
            if !found {
                return Err(Error::Format(format!("item {key:?} not found")));
            }
            f(&mut items[slot].1);
            Ok(())
        })
    }

    pub fn delete(&mut self, root: u64, key: Key) -> Result<(), Error> {
        self.edit_leaf(root, key, |items, slot, found| {
            if !found {
                return Err(Error::Format(format!("item {key:?} not found")));
            }
            items.remove(slot);
            Ok(())
        })
    }

    pub fn insert(&mut self, root: u64, key: Key, data: &[u8]) -> Result<(), Error> {
        self.edit_leaf(root, key, |items, slot, found| {
            if found {
                return Err(Error::Format(format!("item {key:?} already present")));
            }
            items.insert(slot, (key, data.to_vec()));
            Ok(())
        })
    }

    /// Replaces the data of an item (any size).
    pub fn replace(&mut self, root: u64, key: Key, data: &[u8]) -> Result<(), Error> {
        self.edit_leaf(root, key, |items, slot, found| {
            if !found {
                return Err(Error::Format(format!("item {key:?} not found")));
            }
            items[slot].1 = data.to_vec();
            Ok(())
        })
    }

    /// The changed nodes, with their checksums recomputed.
    pub fn finish(self) -> HashMap<u64, Vec<u8>> {
        let mut out = self.changed;
        for n in out.values_mut() {
            let c = csum(&n[32..]);
            n[..32].fill(0);
            n[..4].copy_from_slice(&c);
        }
        out
    }
}
