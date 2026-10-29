//! Read-only access to a single-device btrfs, from the raw device.
//!
//! Given the superblock of generation N (read right after the snapshots), rebuilds the chunk
//! map and, from the extent tree, the list of physical blocks in use at that generation.
//! Every node read is verified (checksum, bytenr, fsid, generation): a node reused after N is
//! reported as [`Error::Changed`] and the caller can retry the run.

pub mod capture;
pub mod cbt;
pub mod device;
pub mod disk;
pub mod edit;
pub mod exclude;
pub mod image;
pub mod prepare;
pub mod snapshot;
pub mod usedmap;

use device::Device;
use disk::*;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Format(String),
    Unsupported(String),
    /// A node no longer matches the superblock generation: the filesystem changed under us
    /// (block reused after the commit).
    Changed { logical: u64, reason: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O: {e}"),
            Error::Format(s) => write!(f, "format: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Changed { logical, reason } => {
                write!(f, "node {logical} changed after the generation read: {reason}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub struct Filesystem {
    dev: Device,
    pub sb: Superblock,
    /// Chunks by starting logical address.
    pub chunks: BTreeMap<u64, Chunk>,
    /// Captured nodes (already verified): they take precedence over reading from the device.
    cache: Option<Arc<HashMap<u64, Vec<u8>>>>,
}

/// Callback of [`Filesystem::walk`] for each leaf item: (node, key, item data).
pub type LeafVisitor<'a> = dyn FnMut(&Node, Key, &[u8]) -> Result<(), Error> + 'a;

/// A node that has been read and verified.
pub struct Node {
    pub header: NodeHeader,
    pub data: Vec<u8>,
}

impl Node {
    /// Items of a leaf: (key, item data).
    pub fn items(&self) -> impl Iterator<Item = (Key, &[u8])> {
        (0..self.header.nritems as usize).map(move |i| {
            let it = &self.data[HEADER_SIZE + i * ITEM_SIZE..];
            let off = HEADER_SIZE + u32_at(it, 17) as usize;
            let size = u32_at(it, 21) as usize;
            (Key::parse(it), &self.data[off..off + size])
        })
    }

    /// Pointers of an internal node: (key, child bytenr, expected generation).
    pub fn ptrs(&self) -> impl Iterator<Item = (Key, u64, u64)> {
        (0..self.header.nritems as usize).map(move |i| {
            let p = &self.data[HEADER_SIZE + i * KEY_PTR_SIZE..];
            (Key::parse(p), u64_at(p, 17), u64_at(p, 25))
        })
    }
}

/// Items (leaf) or key pointers (internal node) inside the node, so that `Node::items` and
/// `Node::ptrs` never index past it.
pub fn node_layout(h: &NodeHeader, data: &[u8]) -> Result<(), String> {
    let n = h.nritems as usize;
    if h.level > 7 {
        return Err(format!("level {}", h.level));
    }
    if h.level > 0 {
        if HEADER_SIZE + n * KEY_PTR_SIZE > data.len() {
            return Err(format!("{n} pointers"));
        }
        return Ok(());
    }
    if HEADER_SIZE + n * ITEM_SIZE > data.len() {
        return Err(format!("{n} items"));
    }
    let room = data.len() - HEADER_SIZE;
    for i in 0..n {
        let it = &data[HEADER_SIZE + i * ITEM_SIZE..];
        let (off, size) = (u32_at(it, 17) as usize, u32_at(it, 21) as usize);
        if off.checked_add(size).is_none_or(|end| end > room) {
            return Err(format!("item {i} at {off}+{size}"));
        }
    }
    Ok(())
}

impl Filesystem {
    pub fn open(path: &Path) -> Result<Filesystem, Error> {
        Filesystem::open_at(path, 0)
    }

    /// A filesystem starting `base` bytes into `path`.
    pub fn open_at(path: &Path, base: u64) -> Result<Filesystem, Error> {
        let dev = Device::open_at(path, base)?;
        let mut raw = vec![0u8; SUPER_SIZE];
        dev.read_exact_at(&mut raw, SUPER_OFFSET)?;
        let sb = Superblock::parse(raw)?;
        let mut fs = Filesystem { dev, sb, chunks: BTreeMap::new(), cache: None };
        fs.load_sys_chunks()?;
        fs.load_chunk_tree()?;
        // Every stripe inside the device: physical offsets index tables sized on it.
        let sector = fs.sb.sectorsize as u64;
        for c in fs.chunks.values() {
            if c.logical % sector != 0 || c.length % sector != 0 {
                return Err(Error::Format(format!("chunk at {} not aligned", c.logical)));
            }
            if let Some(s) = c.stripes.iter().find(|&&s| s % sector != 0 || s.checked_add(c.length).is_none_or(|e| e > fs.sb.total_bytes)) {
                return Err(Error::Format(format!("chunk at {}: stripe at {s} outside the device", c.logical)));
            }
        }
        Ok(fs)
    }

    fn load_sys_chunks(&mut self) -> Result<(), Error> {
        let arr = self.sb.sys_chunk_array().to_vec();
        let mut o = 0;
        while o < arr.len() {
            if o + 17 > arr.len() {
                return Err(Error::Format("truncated sys_chunk_array".into()));
            }
            let key = Key::parse(&arr[o..]);
            o += 17;
            if key.kind != CHUNK_ITEM_KEY {
                return Err(Error::Format("sys_chunk_array with a non-chunk key".into()));
            }
            let (chunk, size) = Chunk::parse(key.offset, &arr[o..], self.sb.devid)?;
            o += size;
            self.chunks.insert(chunk.logical, chunk);
        }
        Ok(())
    }

    fn load_chunk_tree(&mut self) -> Result<(), Error> {
        let mut found = Vec::new();
        self.walk(self.sb.chunk_root, self.sb.chunk_root_generation, &mut |_, key, data| {
            if key.kind == CHUNK_ITEM_KEY {
                found.push((key.offset, data.to_vec()));
            }
            Ok(())
        })?;
        for (logical, data) in found {
            let (chunk, _) = Chunk::parse(logical, &data, self.sb.devid)?;
            self.chunks.insert(logical, chunk);
        }
        Ok(())
    }

    /// Chunk containing the logical address.
    pub fn chunk_of(&self, logical: u64) -> Result<&Chunk, Error> {
        self.chunks
            .range(..=logical)
            .next_back()
            .map(|(_, c)| c)
            .filter(|c| logical < c.logical + c.length)
            .ok_or_else(|| Error::Format(format!("logical address {logical} outside the chunks")))
    }

    /// Physical offsets (one per stripe) of a logical address.
    pub fn physical(&self, logical: u64) -> Result<Vec<u64>, Error> {
        let c = self.chunk_of(logical)?;
        Ok(c.stripes.iter().map(|s| s + (logical - c.logical)).collect())
    }

    pub fn read_at(&self, buf: &mut [u8], physical: u64) -> Result<(), Error> {
        Ok(self.dev.read_exact_at(buf, physical)?)
    }

    /// From now on captured nodes are read from the capture, not from the device.
    pub fn use_capture(&mut self, nodes: Arc<HashMap<u64, Vec<u8>>>) {
        self.cache = Some(nodes);
    }

    /// Reads and verifies a node. `expected_gen` is the generation stored in the parent pointer
    /// (0 = unknown, only checks that it does not exceed the superblock's).
    pub fn read_node(&self, logical: u64, expected_gen: u64) -> Result<Node, Error> {
        if let Some(data) = self.cache.as_ref().and_then(|c| c.get(&logical)) {
            let header = NodeHeader::parse(data);
            if expected_gen != 0 && header.generation != expected_gen {
                return Err(Error::Changed {
                    logical,
                    reason: format!("generation {} instead of {expected_gen}", header.generation),
                });
            }
            return Ok(Node { header, data: data.clone() });
        }
        // With DUP, a bad first copy is no reason to fail: the second one may be good.
        let mut first_err = None;
        for phys in self.physical(logical)? {
            let mut data = vec![0u8; self.sb.nodesize as usize];
            let read = self.read_at(&mut data, phys).and_then(|()| self.verify_node(logical, expected_gen, &data));
            match read {
                Ok(header) => return Ok(Node { header, data }),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        Err(first_err.expect("chunks have at least one stripe"))
    }

    /// Reads and verifies many nodes at once, in parallel (NVMe drives answer many requests at
    /// a time): same results, in the same order, as `read_node` on each.
    pub fn read_nodes(&self, ptrs: &[(u64, u64)]) -> Result<Vec<Node>, Error> {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 16);
        if ptrs.len() < 64 {
            return ptrs.iter().map(|&(l, g)| self.read_node(l, g)).collect();
        }
        let per = ptrs.len().div_ceil(threads);
        std::thread::scope(|s| {
            let parts: Vec<_> = ptrs
                .chunks(per)
                .map(|c| s.spawn(move || c.iter().map(|&(l, g)| self.read_node(l, g)).collect::<Result<Vec<_>, _>>()))
                .collect();
            let mut out = Vec::with_capacity(ptrs.len());
            for p in parts {
                out.extend(p.join().expect("node reader panicked")?);
            }
            Ok(out)
        })
    }

    /// Breadth-first walk of trees, reading each level in parallel. `visit` gets every node
    /// once (shared nodes included only once) and returns the children to descend into.
    pub fn walk_parallel(
        &self,
        roots: &[(u64, u64)],
        visit: &mut dyn FnMut(&Node) -> Result<Vec<(u64, u64)>, Error>,
    ) -> Result<(), Error> {
        let mut seen = std::collections::HashSet::new();
        let mut frontier: Vec<(u64, u64)> = roots.to_vec();
        while !frontier.is_empty() {
            frontier.retain(|(l, _)| seen.insert(*l));
            let mut next = Vec::new();
            for chunk in frontier.chunks(8192) {
                for node in self.read_nodes(chunk)? {
                    next.extend(visit(&node)?);
                }
            }
            frontier = next;
        }
        Ok(())
    }

    /// Checks that `data` is node `logical` of the generation read: checksum, bytenr, fsid,
    /// generation not newer than the superblock (and exactly `expected_gen` if ≠ 0), and that
    /// its items fit in it.
    pub fn verify_node(&self, logical: u64, expected_gen: u64, data: &[u8]) -> Result<NodeHeader, Error> {
        let changed = |reason: String| Error::Changed { logical, reason };
        if data.len() != self.sb.nodesize as usize {
            return Err(changed(format!("{} bytes", data.len())));
        }
        if data[..4] != csum(&data[32..]) {
            return Err(changed("bad checksum".into()));
        }
        let header = NodeHeader::parse(data);
        if header.bytenr != logical {
            return Err(changed(format!("bytenr {} in the header", header.bytenr)));
        }
        if data[32..48] != self.sb.metadata_uuid {
            return Err(changed("fsid mismatch".into()));
        }
        if header.generation > self.sb.generation {
            return Err(changed(format!(
                "generation {} > superblock {}",
                header.generation, self.sb.generation
            )));
        }
        if expected_gen != 0 && header.generation != expected_gen {
            return Err(changed(format!("generation {} instead of {expected_gen}", header.generation)));
        }
        // Checksum right, layout wrong: a corrupt node, not one that changed.
        node_layout(&header, data).map_err(|e| Error::Format(format!("node {logical}: {e}")))?;
        Ok(header)
    }

    /// Depth-first walk of a tree, calling `leaf` for every leaf item.
    pub fn walk(
        &self,
        root: u64,
        root_gen: u64,
        leaf: &mut LeafVisitor,
    ) -> Result<(), Error> {
        let mut stack = vec![(root, root_gen)];
        while let Some((logical, expected_gen)) = stack.pop() {
            let node = self.read_node(logical, expected_gen)?;
            if node.header.level == 0 {
                for (key, data) in node.items() {
                    leaf(&node, key, data)?;
                }
            } else {
                // In reverse order, so leaves are visited in key order.
                let ptrs: Vec<_> = node.ptrs().collect();
                for (_, child, child_gen) in ptrs.into_iter().rev() {
                    stack.push((child, child_gen));
                }
            }
        }
        Ok(())
    }

    /// All ROOT_ITEMs of the root tree: (objectid, item data), at least as long as the
    /// oldest format (239 bytes).
    pub fn root_items(&self) -> Result<Vec<(u64, Vec<u8>)>, Error> {
        let mut out = Vec::new();
        self.walk(self.sb.root, 0, &mut |_, key, data| {
            if key.kind == ROOT_ITEM_KEY {
                if data.len() < ROOT_ITEM_MIN {
                    return Err(Error::Format(format!("root item of {} is {} bytes", key.objectid, data.len())));
                }
                out.push((key.objectid, data.to_vec()));
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// Root of a tree, from the root tree: (bytenr, generation).
    pub fn tree_root(&self, objectid: u64) -> Result<(u64, u64), Error> {
        let mut found = None;
        self.walk(self.sb.root, 0, &mut |_, key, data| {
            if key.objectid == objectid && key.kind == ROOT_ITEM_KEY && found.is_none() {
                found = Some(root_location(data)?);
            }
            Ok(())
        })?;
        found.ok_or_else(|| Error::Format(format!("tree {objectid} missing from the root tree")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(level: u8, nritems: u32) -> NodeHeader {
        NodeHeader { bytenr: 0, generation: 1, owner: 5, nritems, level }
    }

    #[test]
    fn node_layout_bounds() {
        let mut leaf = vec![0u8; 4096];
        let item = |n: &mut Vec<u8>, i: usize, off: u32, size: u32| {
            let h = HEADER_SIZE + i * ITEM_SIZE;
            n[h + 17..h + 21].copy_from_slice(&off.to_le_bytes());
            n[h + 21..h + 25].copy_from_slice(&size.to_le_bytes());
        };
        item(&mut leaf, 0, 3900, 95);
        assert!(node_layout(&header(0, 1), &leaf).is_ok());
        item(&mut leaf, 1, 3900, 96);
        assert!(node_layout(&header(0, 2), &leaf).is_err(), "item past the end");
        item(&mut leaf, 1, u32::MAX, 2);
        assert!(node_layout(&header(0, 2), &leaf).is_err(), "offset overflow");
        assert!(node_layout(&header(0, 200), &leaf).is_err(), "more items than fit");
        assert!(node_layout(&header(1, 121), &leaf).is_ok());
        assert!(node_layout(&header(1, 122), &leaf).is_err(), "more pointers than fit");
        assert!(node_layout(&header(8, 0), &leaf).is_err(), "level");
    }

    /// A btrfs made by mkfs.btrfs in a temporary file (None if mkfs.btrfs is missing).
    pub(crate) fn mkfs(name: &str, args: &[&str]) -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("btrfs-image-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("fs.img");
        std::fs::File::create(&img).unwrap().set_len(256 << 20).unwrap();
        let ok = std::process::Command::new("mkfs.btrfs").args(["-q", "-f"]).args(args).arg(&img).status().is_ok_and(|s| s.success());
        if !ok {
            eprintln!("mkfs.btrfs not available: skipped");
            return None;
        }
        Some(img)
    }

    #[test]
    fn bad_dup_copy_read_from_the_other() {
        use std::os::unix::fs::FileExt;
        let Some(img) = mkfs("dup", &["-m", "dup"]) else { return };
        let fs = Filesystem::open(&img).unwrap();
        let copies = fs.physical(fs.sb.root).unwrap();
        assert_eq!(copies.len(), 2);
        let good = fs.read_node(fs.sb.root, 0).unwrap().data;
        // Corrupt the first copy of the root tree's root.
        let f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        f.write_all_at(&[0xAA; 64], copies[0] + 300).unwrap();
        let fs = Filesystem::open(&img).unwrap();
        assert_eq!(fs.read_node(fs.sb.root, 0).unwrap().data, good);
        // The image gets the good copy in place of the bad one.
        let map = usedmap::UsedMap::build(&fs).unwrap();
        let cap = capture::Capture::default();
        let start = copies[0] / cbt::BLOCK * cbt::BLOCK;
        let mut buf = vec![0u8; cbt::BLOCK as usize];
        fs.read_at(&mut buf, start).unwrap();
        assert!(image::fill_window(&fs, &cap, &map, start, &mut buf).unwrap());
        let o = (copies[0] - start) as usize;
        assert_eq!(&buf[o..o + good.len()], &good[..]);
        let _ = std::fs::remove_dir_all(img.parent().unwrap());
    }
}
