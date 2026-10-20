//! Used blocks of an ext2/3/4 filesystem, read from the raw device: superblock, group
//! descriptors and block bitmaps. Never-used groups (`BLOCK_UNINIT`) have no bitmap on
//! disk: for them only the metadata counts (superblock and descriptor copies, bitmaps and
//! inode tables), as the kernel does when it initializes the bitmap.

use btrfs_image::device::Device;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SB_OFFSET: u64 = 1024;
const MAGIC: u16 = 0xEF53;

const COMPAT_SPARSE_SUPER2: u32 = 0x200;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_64BIT: u32 = 0x80;
const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
const RO_COMPAT_GDT_CSUM: u32 = 0x10;
const RO_COMPAT_BIGALLOC: u32 = 0x200;
const RO_COMPAT_METADATA_CSUM: u32 = 0x400;
const BG_BLOCK_UNINIT: u16 = 0x2;
const BG_INODE_UNINIT: u16 = 0x1;
const INODE_EXTENTS: u32 = 0x80000;
const INODE_INLINE_DATA: u32 = 0x1000_0000;
const EXTENT_MAGIC: u16 = 0xF30A;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Format(String),
    Unsupported(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O: {e}"),
            Error::Format(s) => write!(f, "ext format: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported ext: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

struct Super {
    first_ino: u64,
    blocks: u64,
    first_data_block: u64,
    block_size: u64,
    blocks_per_group: u64,
    inodes_per_group: u64,
    inode_size: u64,
    compat: u32,
    incompat: u32,
    ro_compat: u32,
    reserved_gdt_blocks: u64,
    desc_size: u64,
    first_meta_bg: u64,
    backup_bgs: [u32; 2],
    wtime: u32,
}

impl Super {
    fn parse(b: &[u8]) -> Result<Super, Error> {
        if u16_at(b, 0x38) != MAGIC {
            return Err(Error::Format("superblock magic missing".into()));
        }
        let incompat = u32_at(b, 0x60);
        let is64 = incompat & INCOMPAT_64BIT != 0;
        let log = u32_at(b, 0x18);
        if log > 6 {
            return Err(Error::Format(format!("s_log_block_size {log}")));
        }
        let s = Super {
            first_ino: if u32_at(b, 0x4C) == 0 { 11 } else { u32_at(b, 0x54) as u64 },
            blocks: u32_at(b, 0x4) as u64 | if is64 { (u32_at(b, 0x150) as u64) << 32 } else { 0 },
            first_data_block: u32_at(b, 0x14) as u64,
            block_size: 1024 << log,
            blocks_per_group: u32_at(b, 0x20) as u64,
            inodes_per_group: u32_at(b, 0x28) as u64,
            inode_size: if u32_at(b, 0x4C) == 0 { 128 } else { u16_at(b, 0x58) as u64 },
            compat: u32_at(b, 0x5C),
            incompat,
            ro_compat: u32_at(b, 0x64),
            reserved_gdt_blocks: u16_at(b, 0xCE) as u64,
            desc_size: if is64 { u16_at(b, 0xFE) as u64 } else { 32 },
            first_meta_bg: u32_at(b, 0x104) as u64,
            backup_bgs: [u32_at(b, 0x24C), u32_at(b, 0x250)],
            wtime: u32_at(b, 0x30),
        };
        if s.ro_compat & RO_COMPAT_BIGALLOC != 0 {
            return Err(Error::Unsupported("bigalloc".into()));
        }
        if s.blocks_per_group == 0 || s.blocks_per_group > 8 * s.block_size || s.desc_size < 32 {
            return Err(Error::Format("invalid group geometry".into()));
        }
        Ok(s)
    }

    fn groups(&self) -> u64 {
        (self.blocks - self.first_data_block).div_ceil(self.blocks_per_group)
    }

    fn descs_per_block(&self) -> u64 {
        self.block_size / self.desc_size
    }

    fn group_first_block(&self, g: u64) -> u64 {
        self.first_data_block + g * self.blocks_per_group
    }

    /// The group holds a copy of the superblock (and of the descriptors, without meta_bg).
    fn has_super(&self, g: u64) -> bool {
        if g == 0 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 {
            return self.backup_bgs.iter().any(|&b| b as u64 == g);
        }
        if g <= 1 || self.ro_compat & RO_COMPAT_SPARSE_SUPER == 0 {
            return true;
        }
        if g & 1 == 0 {
            return false;
        }
        [3u64, 5, 7].iter().any(|&b| {
            let mut x = b;
            while x < g {
                x *= b;
            }
            x == g
        })
    }

    /// Block of descriptor-table block number `n`.
    fn descriptor_block(&self, n: u64) -> u64 {
        if self.incompat & INCOMPAT_META_BG == 0 || n < self.first_meta_bg {
            return self.first_data_block + 1 + n;
        }
        let g = n * self.descs_per_block();
        let mut has = self.has_super(g) as u64;
        if g == 0 && self.first_data_block == 0 && self.block_size == 1024 {
            has = 1;
        }
        self.group_first_block(g) + has
    }

    /// Metadata blocks at the start of the group: superblock, descriptors, reserved descriptors.
    fn base_meta_blocks(&self, g: u64) -> Vec<u64> {
        let first = self.group_first_block(g);
        let dpb = self.descs_per_block();
        let meta_bg = self.incompat & INCOMPAT_META_BG != 0;
        let mut out = Vec::new();
        if !meta_bg || g < self.first_meta_bg * dpb {
            if self.has_super(g) {
                let gdt = self.groups().div_ceil(dpb);
                out.extend(first..first + 1 + gdt + self.reserved_gdt_blocks);
            }
        } else {
            let has = self.has_super(g) as u64;
            out.extend(first..first + has);
            let i = g % dpb;
            if i == 0 || i == 1 || i == dpb - 1 {
                out.push(first + has);
            }
        }
        out
    }
}

/// What an incremental needs to know about the inodes: a signature of every inode in use, the
/// blocks each one owns, and the inode table blocks that hold no inode in use.
#[derive(Default)]
pub struct Inodes {
    /// Inode number → (ctime, mtime, size, generation and flags): if it is the same as in the
    /// previous backup, the file and its blocks did not change (the approach of rustic and
    /// restic: ctime cannot be set by a program, any change of content or mapping updates it).
    pub sigs: HashMap<u32, [u64; 4]>,
    /// Byte ranges owned by an inode: its data and the blocks of its mapping (extent tree
    /// nodes, indirect blocks), as (start, length, inode), sorted.
    pub owned: Vec<(u64, u64, u32)>,
    /// Inode table blocks with no inode in use: (start, length, first inode, inode count).
    pub free_itable: Vec<(u64, u64, u32, u32)>,
}

/// An ext filesystem read from the device.
pub struct ExtFs {
    pub block_size: u64,
    /// Filesystem size in bytes (blocks × block size).
    pub size: u64,
    /// Used byte ranges, sorted and merged: (start, length).
    pub ranges: Vec<(u64, u64)>,
    /// Last superblock write time (to detect a changed filesystem).
    pub wtime: u32,
    pub groups: u64,
    path: PathBuf,
    sb: Super,
    descs: Vec<u8>,
}

impl ExtFs {
    pub fn open(path: &Path) -> Result<ExtFs, Error> {
        let dev = Device::open(path)?;
        let mut raw = vec![0u8; 1024];
        dev.read_exact_at(&mut raw, SB_OFFSET)?;
        let sb = Super::parse(&raw)?;
        let bs = sb.block_size;
        let ngroups = sb.groups();
        let uninit_ok = sb.ro_compat & (RO_COMPAT_GDT_CSUM | RO_COMPAT_METADATA_CSUM) != 0;

        // Descriptors of all groups.
        let dpb = sb.descs_per_block();
        let mut descs = Vec::with_capacity((ngroups * sb.desc_size) as usize);
        let mut block = vec![0u8; bs as usize];
        for n in 0..ngroups.div_ceil(dpb) {
            dev.read_exact_at(&mut block, sb.descriptor_block(n) * bs)?;
            descs.extend_from_slice(&block);
        }
        let desc = |g: u64| &descs[(g * sb.desc_size) as usize..((g + 1) * sb.desc_size) as usize];
        let hi = |d: &[u8], o: usize| if sb.desc_size >= 64 { (u32_at(d, o) as u64) << 32 } else { 0 };

        let mut used = vec![0u64; sb.blocks.div_ceil(64) as usize];
        let mut mark = |b: u64| {
            if b < sb.blocks {
                used[(b / 64) as usize] |= 1 << (b % 64);
            }
        };
        // Block 0 (boot sector and, with 4 KiB blocks, the superblock).
        mark(0);
        let itable_blocks = (sb.inodes_per_group * sb.inode_size).div_ceil(bs);
        for g in 0..ngroups {
            let d = desc(g);
            let block_bitmap = u32_at(d, 0x0) as u64 | hi(d, 0x20);
            let inode_bitmap = u32_at(d, 0x4) as u64 | hi(d, 0x24);
            let inode_table = u32_at(d, 0x8) as u64 | hi(d, 0x28);
            let flags = u16_at(d, 0x12);
            mark(block_bitmap);
            mark(inode_bitmap);
            for b in inode_table..inode_table + itable_blocks {
                mark(b);
            }
            for b in sb.base_meta_blocks(g) {
                mark(b);
            }
            if uninit_ok && flags & BG_BLOCK_UNINIT != 0 {
                continue;
            }
            if block_bitmap == 0 || block_bitmap >= sb.blocks {
                return Err(Error::Format(format!("group {g}: block bitmap at {block_bitmap}")));
            }
            dev.read_exact_at(&mut block, block_bitmap * bs)?;
            let first = sb.group_first_block(g);
            let n = sb.blocks_per_group.min(sb.blocks - first);
            for i in 0..n {
                if block[(i / 8) as usize] & (1 << (i % 8)) != 0 {
                    mark(first + i);
                }
            }
        }

        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for (w, &bits) in used.iter().enumerate() {
            if bits == 0 {
                continue;
            }
            for i in 0..64 {
                if bits & (1 << i) != 0 {
                    let b = w as u64 * 64 + i;
                    match ranges.last_mut() {
                        Some((s, l)) if *s + *l == b * bs => *l += bs,
                        _ => ranges.push((b * bs, bs)),
                    }
                }
            }
        }
        Ok(ExtFs { block_size: bs, size: sb.blocks * bs, ranges, wtime: sb.wtime, groups: ngroups, path: path.to_path_buf(), sb, descs })
    }

    /// Reads the inode tables (only the blocks holding inodes in use) and the mapping of every
    /// inode. Special inodes (journal, resize…) are left out: their blocks are always read.
    pub fn inodes(&self) -> Result<Inodes, Error> {
        let sb = &self.sb;
        let bs = sb.block_size;
        let dev = Device::open(&self.path)?;
        let uninit_ok = sb.ro_compat & (RO_COMPAT_GDT_CSUM | RO_COMPAT_METADATA_CSUM) != 0;
        let per_block = bs / sb.inode_size;
        let itable_blocks = (sb.inodes_per_group * sb.inode_size).div_ceil(bs);
        let mut out = Inodes::default();
        let mut bitmap = vec![0u8; bs as usize];
        for g in 0..sb.groups() {
            let d = &self.descs[(g * sb.desc_size) as usize..((g + 1) * sb.desc_size) as usize];
            let hi = |o: usize| if sb.desc_size >= 64 { (u32_at(d, o) as u64) << 32 } else { 0 };
            let hi16 = |o: usize| if sb.desc_size >= 64 { (u16_at(d, o) as u64) << 16 } else { 0 };
            let inode_bitmap = u32_at(d, 0x4) as u64 | hi(0x24);
            let table = u32_at(d, 0x8) as u64 | hi(0x28);
            let flags = u16_at(d, 0x12);
            let first_ino = (g * sb.inodes_per_group + 1) as u32;
            // Inodes possibly in use in this group.
            let in_use = if uninit_ok && flags & BG_INODE_UNINIT != 0 {
                0
            } else {
                let unused = if uninit_ok { u16_at(d, 0x1C) as u64 | hi16(0x32) } else { 0 };
                sb.inodes_per_group.saturating_sub(unused)
            };
            if in_use > 0 {
                dev.read_exact_at(&mut bitmap, inode_bitmap * bs)?;
            }
            let used = |i: u64| i < in_use && bitmap[(i / 8) as usize] & (1 << (i % 8)) != 0;
            let mut block = vec![0u8; bs as usize];
            for b in 0..itable_blocks {
                let range = b * per_block..((b + 1) * per_block).min(sb.inodes_per_group);
                if !range.clone().any(used) {
                    out.free_itable.push(((table + b) * bs, bs, first_ino + range.start as u32, (range.end - range.start) as u32));
                    continue;
                }
                dev.read_exact_at(&mut block, (table + b) * bs)?;
                for i in range.filter(|&i| used(i)) {
                    let ino = first_ino + i as u32;
                    if (ino as u64) < sb.first_ino && ino != 2 {
                        continue;
                    }
                    let o = ((i % per_block) * sb.inode_size) as usize;
                    let raw = &block[o..o + sb.inode_size as usize];
                    if let Some((sig, owned)) = inode(raw, sb, &dev) {
                        out.sigs.insert(ino, sig);
                        out.owned.extend(owned.into_iter().map(|(s, l)| (s * bs, l * bs, ino)));
                    }
                }
            }
        }
        out.owned.sort_unstable();
        Ok(out)
    }

    pub fn used_bytes(&self) -> u64 {
        self.ranges.iter().map(|r| r.1).sum()
    }

    /// Zeroes the unused areas in `buf` (volume bytes from `start`); `false` if the window
    /// contains no used bytes.
    pub fn fill_window(&self, start: u64, buf: &mut [u8]) -> bool {
        let end = start + buf.len() as u64;
        let k = self.ranges.partition_point(|(s, l)| s + l <= start);
        let mut cursor = start;
        let mut any = false;
        for &(s, l) in &self.ranges[k..] {
            if s >= end {
                break;
            }
            let (rs, re) = (s.max(start), (s + l).min(end));
            any = true;
            if rs > cursor {
                buf[(cursor - start) as usize..(rs - start) as usize].fill(0);
            }
            cursor = re;
        }
        if cursor < end {
            buf[(cursor - start) as usize..].fill(0);
        }
        any
    }

    /// Whether the window `[start, end)` contains used bytes.
    pub fn any_used(&self, start: u64, end: u64) -> bool {
        let k = self.ranges.partition_point(|(s, l)| s + l <= start);
        self.ranges.get(k).is_some_and(|(s, _)| *s < end)
    }
}

/// Signature and owned blocks (first block, count) of an inode in use; `None` if it cannot be
/// read with confidence (its blocks are then always read).
fn inode(raw: &[u8], sb: &Super, dev: &Device) -> Option<([u64; 4], Vec<(u64, u64)>)> {
    let mode = u16_at(raw, 0) as u32;
    if mode == 0 || u16_at(raw, 0x1A) == 0 {
        return None;
    }
    let extra = if raw.len() > 0x80 { u16_at(raw, 0x80) as usize } else { 0 };
    let field = |o: usize| if extra >= o + 4 - 0x80 && raw.len() >= o + 4 { u32_at(raw, o) as u64 } else { 0 };
    let flags = u32_at(raw, 0x20);
    let size = u32_at(raw, 4) as u64 | (u32_at(raw, 0x6C) as u64) << 32;
    let sig = [
        (u32_at(raw, 0xC) as u64) << 32 | field(0x84),
        (u32_at(raw, 0x10) as u64) << 32 | field(0x88),
        size,
        (u32_at(raw, 0x64) as u64) << 32 | flags as u64,
    ];
    let kind = mode & 0xF000;
    let mut owned = Vec::new();
    // Device files, FIFOs and sockets own no block; neither do inline data and fast symlinks.
    let has_blocks = matches!(kind, 0x8000 | 0x4000 | 0xA000) && flags & INODE_INLINE_DATA == 0 && u32_at(raw, 0x1C) != 0;
    if has_blocks {
        let iblock = &raw[0x28..0x28 + 60];
        let ok = if flags & INODE_EXTENTS != 0 {
            extents(iblock, sb, dev, &mut owned, 0)
        } else {
            indirect(iblock, sb, dev, &mut owned)
        };
        if !ok {
            return None;
        }
    }
    Some((sig, owned))
}

fn extents(node: &[u8], sb: &Super, dev: &Device, owned: &mut Vec<(u64, u64)>, level: u32) -> bool {
    if u16_at(node, 0) != EXTENT_MAGIC || level > 5 {
        return false;
    }
    let (entries, depth) = (u16_at(node, 2) as usize, u16_at(node, 6));
    if 12 + entries * 12 > node.len() {
        return false;
    }
    for e in 0..entries {
        let x = &node[12 + e * 12..24 + e * 12];
        if depth == 0 {
            let len = u16_at(x, 4) as u64;
            let len = if len > 32768 { len - 32768 } else { len };
            let start = (u16_at(x, 6) as u64) << 32 | u32_at(x, 8) as u64;
            if start + len > sb.blocks {
                return false;
            }
            owned.push((start, len));
        } else {
            let child = (u16_at(x, 8) as u64) << 32 | u32_at(x, 4) as u64;
            if child >= sb.blocks {
                return false;
            }
            let mut buf = vec![0u8; sb.block_size as usize];
            if dev.read_exact_at(&mut buf, child * sb.block_size).is_err() {
                return false;
            }
            owned.push((child, 1));
            if !extents(&buf, sb, dev, owned, level + 1) {
                return false;
            }
        }
    }
    true
}

/// ext2/3 block map: 12 direct blocks, then single, double and triple indirect blocks.
fn indirect(iblock: &[u8], sb: &Super, dev: &Device, owned: &mut Vec<(u64, u64)>) -> bool {
    fn walk(b: u64, depth: u32, sb: &Super, dev: &Device, owned: &mut Vec<(u64, u64)>) -> bool {
        if b == 0 {
            return true;
        }
        if b >= sb.blocks {
            return false;
        }
        match owned.last_mut() {
            Some((s, l)) if *s + *l == b => *l += 1,
            _ => owned.push((b, 1)),
        }
        if depth == 0 {
            return true;
        }
        let mut buf = vec![0u8; sb.block_size as usize];
        if dev.read_exact_at(&mut buf, b * sb.block_size).is_err() {
            return false;
        }
        (0..buf.len() / 4).all(|i| walk(u32_at(&buf, i * 4) as u64, depth - 1, sb, dev, owned))
    }
    (0..15).all(|i| walk(u32_at(iblock, i * 4) as u64, i.saturating_sub(11) as u32, sb, dev, owned))
}
