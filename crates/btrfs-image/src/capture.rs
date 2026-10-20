//! Capture of the blocks that snapshots do not protect.
//!
//! After the snapshots and the superblock read (generation N), these remain exposed to reuse:
//! - the nodes of the global trees (root, chunk, extent, csum, free space, dev, uuid, …);
//! - the root node of every subvolume (the snapshot has a copy of it, it does not share it);
//! - the nodes of a subvolume modified after its snapshot (generation > the snapshot's
//!   otransid) and the data extents written in that window.
//!
//! These blocks must be read right away, verified and set aside; everything else is shared
//! with a snapshot and stays put as long as the snapshots exist.

use crate::disk::*;
use crate::{Error, Filesystem};
use std::collections::HashMap;
use std::sync::Arc;

const EXTENT_DATA_KEY: u8 = 108;
const FILE_EXTENT_INLINE: u8 = 0;

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
    capture_tree(fs, &mut cap, fs.sb.root, 0, None)?;
    fs.walk(fs.sb.root, 0, &mut |_, key, data| {
        if key.kind == ROOT_ITEM_KEY {
            roots.push((key.objectid, u64_at(data, 176), u64_at(data, 160)));
        }
        Ok(())
    })?;
    capture_tree(fs, &mut cap, fs.sb.chunk_root, fs.sb.chunk_root_generation, None)?;

    for (id, bytenr, generation) in roots {
        if !is_subvolume(id) {
            capture_tree(fs, &mut cap, bytenr, generation, None)?;
        } else if snapshots.contains(&id) {
            continue;
        } else if let Some(p) = pinned.iter().find(|p| p.subvol == id) {
            capture_tree(fs, &mut cap, bytenr, generation, Some(p.snapshot_transid))?;
        } else if unpinnable.contains(&id) || id == 5 {
            // Tree captured in full, data excluded (for the top level, which is not snapshotted,
            // it is usually tiny: it only holds the directories containing the subvolumes).
            capture_tree(fs, &mut cap, bytenr, generation, None)?;
        } else {
            return Err(Error::Changed { logical: bytenr, reason: format!("subvolume {id} has no snapshot") });
        }
    }
    Ok(Capture { nodes: Arc::new(cap.nodes), data: cap.data, data_bytes: cap.data_bytes })
}

#[derive(Default)]
struct Building {
    nodes: HashMap<u64, Vec<u8>>,
    data: HashMap<u64, Vec<u8>>,
    data_bytes: u64,
}

/// Captures a tree. With `since = Some(t)` it only descends into nodes with generation > t (the
/// others are shared with the snapshot) and captures the data extents written after t.
fn capture_tree(
    fs: &Filesystem,
    cap: &mut Building,
    root: u64,
    root_gen: u64,
    since: Option<u64>,
) -> Result<(), Error> {
    let mut stack = vec![(root, root_gen)];
    while let Some((logical, expected_gen)) = stack.pop() {
        if cap.nodes.contains_key(&logical) {
            continue;
        }
        let node = fs.read_node(logical, expected_gen)?;
        if node.header.level > 0 {
            for (_, child, child_gen) in node.ptrs() {
                if since.is_none_or(|t| child_gen > t) {
                    stack.push((child, child_gen));
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
    Ok(())
}
