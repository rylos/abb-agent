//! Physical map of the bytes in use at a generation, derived from the extent tree.

use crate::disk::*;
use crate::{Error, Filesystem};

/// What a physical range contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// First MiB of the device (reserved, holds the primary superblock) and superblock copies.
    Reserved,
    /// Tree node (metadata or system).
    Tree,
    Data,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub physical: u64,
    pub len: u64,
    pub kind: Kind,
    /// btrfs logical address (0 for [`Kind::Reserved`]).
    pub logical: u64,
    /// Transaction that allocated the extent (from the extent item; 0 for [`Kind::Reserved`]).
    pub generation: u64,
    /// Non-zero when the bytes sent for this range differ from the disk (tree nodes edited for
    /// the image): mixed into the change fingerprints.
    pub salt: u64,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub tree_blocks: u64,
    pub data_extents: u64,
    pub data_bytes: u64,
    pub tree_bytes_logical: u64,
    pub physical_bytes: u64,
}

pub struct UsedMap {
    /// Data extents left out of the image (excluded subvolumes): (logical, length), sorted.
    pub excluded_data: Vec<(u64, u64)>,
    /// Physical ranges sorted by offset (not merged: each keeps its own kind).
    pub ranges: Vec<Range>,
    pub stats: Stats,
}

impl UsedMap {
    pub fn build(fs: &Filesystem) -> Result<UsedMap, Error> {
        let nodesize = fs.sb.nodesize as u64;
        let mut ranges = Vec::new();
        let mut stats = Stats::default();

        ranges.push(Range { physical: 0, len: 1 << 20, kind: Kind::Reserved, logical: 0, generation: 0, salt: 0 });
        for &m in &SUPER_MIRRORS[1..] {
            if m + SUPER_SIZE as u64 <= fs.sb.total_bytes {
                ranges.push(Range { physical: m, len: SUPER_SIZE as u64, kind: Kind::Reserved, logical: 0, generation: 0, salt: 0 });
            }
        }

        let (extent_root, extent_gen) = fs.tree_root(EXTENT_TREE_OBJECTID)?;
        let mut extents: Vec<(u64, u64, Kind, u64)> = Vec::new();
        fs.walk(extent_root, extent_gen, &mut |_, key, data| {
            match key.kind {
                METADATA_ITEM_KEY => extents.push((key.objectid, nodesize, Kind::Tree, u64_at(data, 8))),
                EXTENT_ITEM_KEY => {
                    let flags = u64_at(data, 16);
                    let kind = if flags & EXTENT_FLAG_TREE_BLOCK != 0 { Kind::Tree } else { Kind::Data };
                    extents.push((key.objectid, key.offset, kind, u64_at(data, 8)));
                }
                _ => {}
            }
            Ok(())
        })?;

        for (logical, len, kind, generation) in extents {
            match kind {
                Kind::Tree => {
                    stats.tree_blocks += 1;
                    stats.tree_bytes_logical += len;
                }
                _ => {
                    stats.data_extents += 1;
                    stats.data_bytes += len;
                }
            }
            for physical in fs.physical(logical)? {
                ranges.push(Range { physical, len, kind, logical, generation, salt: 0 });
            }
        }

        ranges.sort_by_key(|r| r.physical);
        stats.physical_bytes = ranges.iter().map(|r| r.len).sum();
        Ok(UsedMap { ranges, stats, excluded_data: Vec::new() })
    }

    /// Leaves out data extents (sent as unused) and marks edited tree nodes.
    pub fn apply_exclusion(&mut self, data: &[(u64, u64)], edited: &std::collections::HashMap<u64, Vec<u8>>) {
        self.ranges.retain(|r| r.kind != Kind::Data || data.binary_search_by_key(&r.logical, |d| d.0).is_err());
        for r in self.ranges.iter_mut().filter(|r| r.kind == Kind::Tree) {
            if let Some(n) = edited.get(&r.logical) {
                r.salt = (crc32c::crc32c(n) as u64) << 32 | crc32c::crc32c_append(0x5bd1_e995, n) as u64 | 1;
            }
        }
        self.stats.physical_bytes = self.ranges.iter().map(|r| r.len).sum();
        self.excluded_data = data.to_vec();
    }

    /// Bitmap of `block`-byte blocks: true if the block contains at least one used byte.
    pub fn bitmap(&self, device_size: u64, block: u64) -> Vec<bool> {
        let mut bits = vec![false; device_size.div_ceil(block) as usize];
        for r in &self.ranges {
            let first = r.physical / block;
            let last = (r.physical + r.len - 1) / block;
            for b in first..=last {
                bits[b as usize] = true;
            }
        }
        bits
    }
}
