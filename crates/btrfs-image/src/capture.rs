//! Capture of the blocks that snapshots do not protect.
//!
//! After the snapshots and the superblock read (generation N), these remain exposed to reuse:
//! - the nodes of the global trees (root, chunk, extent, csum, free space, dev, uuid, …);
//! - the root node of every subvolume (the snapshot has a copy of it, it does not share it);
//! - the nodes of a subvolume modified after its snapshot (generation > the snapshot's
//!   otransid) and the data extents written in that window.
//!
//! - the data of files deleted but still open (orphan inodes): btrfs drops them from a new
//!   snapshot right after creating it, so only the live subvolume references them, whatever
//!   their age (e.g. a browser's memory-mapped metrics file, rewritten all the time).
//!
//! These blocks must be read right away, verified and set aside; everything else is shared
//! with a snapshot and stays put as long as the snapshots exist.

use crate::disk::*;
use crate::{Error, Filesystem};
use std::collections::HashMap;
use std::sync::Arc;

const EXTENT_DATA_KEY: u8 = 108;
const FILE_EXTENT_INLINE: u8 = 0;
const ORPHAN_OBJECTID: u64 = u64::MAX - 4;
const ORPHAN_ITEM_KEY: u8 = 48;
/// Beyond this, data of deleted open files is not captured (it may change during the upload).
const ORPHAN_DATA_LIMIT: u64 = 1 << 30;

/// A "live" subvolume and the transaction of its snapshot.
#[derive(Clone, Copy, Debug)]
pub struct Pinned {
    pub subvol: u64,
    /// Transaction in which the snapshot protecting it was created.
    pub snapshot_transid: u64,
}

#[derive(Default)]
pub struct Capture {
    /// Nodes by logical address (shared with `Filesystem::use_capture`).
    pub nodes: Arc<HashMap<u64, Vec<u8>>>,
    /// Data extents by starting logical address (whole extent).
    pub data: HashMap<u64, Vec<u8>>,
    pub data_bytes: u64,
}

impl Capture {
    pub fn node_bytes(&self) -> u64 {
        self.nodes.values().map(|n| n.len() as u64).sum()
    }
}

fn is_subvolume(id: u64) -> bool {
    id == 5 || (256..=u64::MAX - 256).contains(&id)
}

/// `pinned`: subvolumes protected by a snapshot created in this run. `snapshots`: the ids of
/// the snapshots themselves (immutable by construction). `unpinnable`: subvolumes that cannot
/// be snapshotted (active swapfile): their nodes are captured, not their data. Any other
/// subvolume is an error: it was created after the listing and has no snapshot protecting it.
pub fn capture(fs: &Filesystem, pinned: &[Pinned], snapshots: &[u64], unpinnable: &[u64]) -> Result<Capture, Error> {
    let mut cap = Building::default();

    // Roots of all trees from the root tree (which is captured in full).
    let mut roots: Vec<(u64, u64, u64)> = Vec::new(); // (objectid, bytenr, generation)
    fs.walk(fs.sb.root, 0, &mut |_, key, data| {
        if key.kind == ROOT_ITEM_KEY {
            roots.push((key.objectid, u64_at(data, 176), u64_at(data, 160)));
        }
        Ok(())
    })?;
    // (node, generation, since): `since` as in `capture_tree`.
    let mut start: Vec<(u64, u64, Option<u64>)> = vec![(fs.sb.root, 0, None), (fs.sb.chunk_root, fs.sb.chunk_root_generation, None)];
    for (id, bytenr, generation) in roots {
        if !is_subvolume(id) {
            start.push((bytenr, generation, None));
        } else if snapshots.contains(&id) {
            continue;
        } else if let Some(p) = pinned.iter().find(|p| p.subvol == id) {
            start.push((bytenr, generation, Some(p.snapshot_transid)));
        } else if unpinnable.contains(&id) || id == 5 {
            // Tree captured in full, data excluded (for the top level, which is not snapshotted,
            // it is usually tiny: it only holds the directories containing the subvolumes).
            start.push((bytenr, generation, None));
        } else {
            return Err(Error::Changed { logical: bytenr, reason: format!("subvolume {id} has no snapshot") });
        }
    }
    capture_trees(fs, &mut cap, start)?;

    // Deleted files still open in the live subvolumes.
    let mut orphan_bytes = 0u64;
    for p in pinned {
        let (bytenr, generation) = fs.tree_root(p.subvol)?;
        let mut inodes = Vec::new();
        walk_range(fs, bytenr, generation, Key { objectid: ORPHAN_OBJECTID, kind: ORPHAN_ITEM_KEY, offset: 0 }, Key { objectid: ORPHAN_OBJECTID, kind: ORPHAN_ITEM_KEY, offset: u64::MAX }, &mut |key, _| inodes.push(key.offset))?;
        for ino in inodes {
            let mut extents = Vec::new();
            walk_range(fs, bytenr, generation, Key { objectid: ino, kind: EXTENT_DATA_KEY, offset: 0 }, Key { objectid: ino, kind: EXTENT_DATA_KEY, offset: u64::MAX }, &mut |_, item| {
                if item.len() >= 37 && item[20] != FILE_EXTENT_INLINE && u64_at(item, 21) != 0 {
                    extents.push((u64_at(item, 21), u64_at(item, 29)));
                }
            })?;
            for (disk_bytenr, disk_num_bytes) in extents {
                if cap.data.contains_key(&disk_bytenr) {
                    continue;
                }
                if orphan_bytes + disk_num_bytes > ORPHAN_DATA_LIMIT {
                    eprintln!("subvolume {}: deleted open file {ino} too big to capture, its data may change during the backup", p.subvol);
                    break;
                }
                let mut buf = vec![0u8; disk_num_bytes as usize];
                fs.read_at(&mut buf, fs.physical(disk_bytenr)?[0])?;
                orphan_bytes += disk_num_bytes;
                cap.data_bytes += disk_num_bytes;
                cap.data.insert(disk_bytenr, buf);
            }
        }
    }
    if orphan_bytes > 0 {
        eprintln!("captured {:.1} MiB of deleted files still open", orphan_bytes as f64 / (1u64 << 20) as f64);
    }
    Ok(Capture { nodes: Arc::new(cap.nodes), data: cap.data, data_bytes: cap.data_bytes })
}

/// Calls `item` for the leaf items of a tree with keys in `lo..=hi`, descending only into the
/// subtrees that can hold them.
fn walk_range(fs: &Filesystem, root: u64, root_gen: u64, lo: Key, hi: Key, item: &mut dyn FnMut(Key, &[u8])) -> Result<(), Error> {
    let mut stack = vec![(root, root_gen)];
    while let Some((logical, generation)) = stack.pop() {
        let node = fs.read_node(logical, generation)?;
        if node.header.level == 0 {
            for (key, data) in node.items() {
                if key >= lo && key <= hi {
                    item(key, data);
                }
            }
            continue;
        }
        let ptrs: Vec<_> = node.ptrs().collect();
        for (i, (key, child, child_gen)) in ptrs.iter().enumerate().rev() {
            // Child i holds the keys from its own up to the next child's.
            let next = ptrs.get(i + 1).map(|p| p.0);
            if *key <= hi && next.is_none_or(|n| n > lo) {
                stack.push((*child, *child_gen));
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Building {
    nodes: HashMap<u64, Vec<u8>>,
    data: HashMap<u64, Vec<u8>>,
    data_bytes: u64,
}

/// Captures trees, level by level with parallel reads. With `since = Some(t)` only the nodes
/// with generation > t are descended into (the others are shared with the snapshot) and the
/// data extents written after t are captured.
fn capture_trees(fs: &Filesystem, cap: &mut Building, roots: Vec<(u64, u64, Option<u64>)>) -> Result<(), Error> {
    let mut frontier = roots;
    while !frontier.is_empty() {
        frontier.retain(|(l, _, _)| !cap.nodes.contains_key(l));
        frontier.sort_by_key(|f| f.0);
        frontier.dedup_by_key(|f| f.0);
        let mut next = Vec::new();
        for chunk in frontier.chunks(8192) {
            let ptrs: Vec<(u64, u64)> = chunk.iter().map(|&(l, g, _)| (l, g)).collect();
            let nodes = fs.read_nodes(&ptrs)?;
            for (node, &(logical, _, since)) in nodes.into_iter().zip(chunk) {
                if node.header.level > 0 {
                    for (_, child, child_gen) in node.ptrs() {
                        if since.is_none_or(|t| child_gen > t) {
                            next.push((child, child_gen, since));
                        }
                    }
                } else if let Some(t) = since {
                    for (key, item) in node.items() {
                        if key.kind != EXTENT_DATA_KEY || item[20] == FILE_EXTENT_INLINE {
                            continue;
                        }
                        let generation = u64_at(item, 0);
                        let disk_bytenr = u64_at(item, 21);
                        let disk_num_bytes = u64_at(item, 29);
                        if generation > t && disk_bytenr != 0 && !cap.data.contains_key(&disk_bytenr) {
                            let mut buf = vec![0u8; disk_num_bytes as usize];
                            fs.read_at(&mut buf, fs.physical(disk_bytenr)?[0])?;
                            cap.data_bytes += disk_num_bytes;
                            cap.data.insert(disk_bytenr, buf);
                        }
                    }
                }
                cap.nodes.insert(logical, node.data);
            }
        }
        frontier = next;
    }
    Ok(())
}
