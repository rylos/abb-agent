//! Used blocks of an ext2/3/4 filesystem, read from the raw device: superblock, group
//! descriptors and block bitmaps. Never-used groups (`BLOCK_UNINIT`) have no bitmap on
//! disk: for them only the metadata counts (superblock and descriptor copies, bitmaps and
//! inode tables), as the kernel does when it initializes the bitmap.

use btrfs_image::device::Device;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SB_OFFSET: u64 = 1024;
const MAGIC: u16 = 0xEF53;

const COMPAT_HAS_JOURNAL: u32 = 0x4;
const COMPAT_SPARSE_SUPER2: u32 = 0x200;
const COMPAT_ORPHAN_FILE: u32 = 0x1000;
const INCOMPAT_COMPRESSION: u32 = 0x1;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_MMP: u32 = 0x100;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_EA_INODE: u32 = 0x400;
const INCOMPAT_DIRDATA: u32 = 0x1000;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const INCOMPAT_ENCRYPT: u32 = 0x10000;
const INCOMPAT_CASEFOLD: u32 = 0x20000;
/// Incompatible features that leave the block bitmaps, group descriptors and block maps as this
/// reader expects them (directory formats, xattrs, encryption, MMP do not matter here). Any
/// other bit, including ones unknown today, is refused rather than misread.
const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_META_BG
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_MMP
    | INCOMPAT_FLEX_BG
    | INCOMPAT_EA_INODE
    | INCOMPAT_DIRDATA
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR
    | INCOMPAT_INLINE_DATA
    | INCOMPAT_ENCRYPT
    | INCOMPAT_CASEFOLD;
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
    /// Inode of the journal (0: none, or on another device).
    journal_inum: u32,
    /// Other inodes the kernel writes without updating their times (orphan file, quota files),
    /// when they are not among the reserved ones.
    internal: Vec<u32>,
}

impl Super {
    fn parse(b: &[u8]) -> Result<Super, Error> {
        if b.len() < 1024 || u16_at(b, 0x38) != MAGIC {
            return Err(Error::Format("superblock magic missing".into()));
        }
        let incompat = u32_at(b, 0x60);
        if incompat & INCOMPAT_JOURNAL_DEV != 0 {
            return Err(Error::Unsupported("external journal device, not a filesystem".into()));
        }
        if incompat & INCOMPAT_COMPRESSION != 0 {
            return Err(Error::Unsupported("compression".into()));
        }
        if incompat & !INCOMPAT_SUPPORTED != 0 {
            return Err(Error::Unsupported(format!("incompatible features 0x{:x}", incompat & !INCOMPAT_SUPPORTED)));
        }
        let is64 = incompat & INCOMPAT_64BIT != 0;
        let log = u32_at(b, 0x18);
        if log > 6 {
            return Err(Error::Format(format!("s_log_block_size {log}")));
        }
        let dynamic = u32_at(b, 0x4C) != 0;
        let compat = u32_at(b, 0x5C);
        let internal = [
            (compat & COMPAT_ORPHAN_FILE != 0).then(|| u32_at(b, 0x280)),
            Some(u32_at(b, 0x240)),
            Some(u32_at(b, 0x244)),
            Some(u32_at(b, 0x26C)),
        ];
        let s = Super {
            first_ino: if dynamic { u32_at(b, 0x54) as u64 } else { 11 },
            blocks: u32_at(b, 0x4) as u64 | if is64 { (u32_at(b, 0x150) as u64) << 32 } else { 0 },
            first_data_block: u32_at(b, 0x14) as u64,
            block_size: 1024 << log,
            blocks_per_group: u32_at(b, 0x20) as u64,
            inodes_per_group: u32_at(b, 0x28) as u64,
            inode_size: if dynamic { u16_at(b, 0x58) as u64 } else { 128 },
            compat,
            incompat,
            ro_compat: u32_at(b, 0x64),
            reserved_gdt_blocks: u16_at(b, 0xCE) as u64,
            desc_size: if is64 { u16_at(b, 0xFE) as u64 } else { 32 },
            first_meta_bg: u32_at(b, 0x104) as u64,
            backup_bgs: [u32_at(b, 0x24C), u32_at(b, 0x250)],
            wtime: u32_at(b, 0x30),
            journal_inum: if compat & COMPAT_HAS_JOURNAL != 0 { u32_at(b, 0xE0) } else { 0 },
            internal: internal.into_iter().flatten().filter(|&i| i != 0).collect(),
        };
        if s.ro_compat & RO_COMPAT_BIGALLOC != 0 {
            return Err(Error::Unsupported("bigalloc".into()));
        }
        // The kernel's own checks (ext4_fill_super): everything below is used to size buffers
        // and index them.
        let bs = s.block_size;
        if s.blocks_per_group == 0 || s.blocks_per_group > 8 * bs {
            return Err(Error::Format("invalid group geometry".into()));
        }
        if s.inodes_per_group == 0 || s.inodes_per_group > 8 * bs {
            return Err(Error::Format(format!("{} inodes per group", s.inodes_per_group)));
        }
        if s.inode_size < 128 || s.inode_size > bs || !s.inode_size.is_power_of_two() {
            return Err(Error::Format(format!("inode size {}", s.inode_size)));
        }
        if is64 && (s.desc_size < 64 || s.desc_size > 1024 || !s.desc_size.is_power_of_two()) {
            return Err(Error::Format(format!("descriptor size {}", s.desc_size)));
        }
        if s.first_ino < 11 {
            return Err(Error::Format(format!("first inode {}", s.first_ino)));
        }
        if s.first_data_block >= s.blocks || s.blocks.checked_mul(bs).is_none() {
            return Err(Error::Format(format!("{} blocks from block {}", s.blocks, s.first_data_block)));
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
                // With meta_bg only the first s_first_meta_bg descriptor blocks are here
                // (ext4_bg_num_gdb_nometa).
                let gdt = if meta_bg { self.first_meta_bg } else { self.groups().div_ceil(dpb) };
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
    /// Inode number → (ctime, hash of mtime and block mapping, size, generation and flags): if
    /// it is the same as in the previous backup, the file and its blocks did not change (the
    /// approach of rustic and restic: ctime cannot be set by a program, any change of content
    /// updates it). The mapping is part of it because blocks can move without a new ctime
    /// (e4defrag, resize2fs, e2fsck cloning duplicate blocks).
    pub sigs: HashMap<u32, [u64; 4]>,
    /// Byte ranges owned by an inode: its data and the blocks of its mapping (extent tree
    /// nodes, indirect blocks), as (start, length, inode), sorted. Inodes the kernel writes
    /// without new times (journal, orphan file, quota files) own nothing here: their blocks are
    /// always read.
    pub owned: Vec<(u64, u64, u32)>,
    /// Inode table blocks with no inode in use: (start, length, first inode, inode count).
    pub free_itable: Vec<(u64, u64, u32, u32)>,
    /// Data of regular files, as byte ranges (start, length), sorted: everything else in use is
    /// metadata.
    pub file_data: Vec<(u64, u64)>,
    /// Blocks of the journal (inode 8), as byte ranges.
    pub journal: Vec<(u64, u64)>,
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
    /// The journal holds transactions not yet written to their place (unclean shutdown, not
    /// mounted since): bitmaps and inode tables on the device are out of date.
    pub needs_recovery: bool,
    path: PathBuf,
    sb: Super,
    descs: Vec<u8>,
    /// Every block counted as used (see `assume_all_used`).
    all_used: bool,
}

impl ExtFs {
    pub fn open(path: &Path) -> Result<ExtFs, Error> {
        let dev = Device::open(path)?;
        let mut raw = vec![0u8; 1024];
        dev.read_exact_at(&mut raw, SB_OFFSET)?;
        let sb = Super::parse(&raw)?;
        let bs = sb.block_size;
        // The last block must be on the device before anything is sized on the block count.
        let mut block = vec![0u8; bs as usize];
        dev.read_exact_at(&mut block, (sb.blocks - 1) * bs)
            .map_err(|e| Error::Format(format!("{} blocks of {bs} bytes: last one unreadable ({e})", sb.blocks)))?;
        let ngroups = sb.groups();
        let uninit_ok = sb.ro_compat & (RO_COMPAT_GDT_CSUM | RO_COMPAT_METADATA_CSUM) != 0;

        // Descriptors of all groups.
        let dpb = sb.descs_per_block();
        let mut descs = Vec::with_capacity((ngroups * sb.desc_size) as usize);
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
            if inode_table.checked_add(itable_blocks).is_none_or(|end| end > sb.blocks) {
                return Err(Error::Format(format!("group {g}: inode table at {inode_table}")));
            }
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
        Ok(ExtFs {
            block_size: bs,
            size: sb.blocks * bs,
            ranges,
            wtime: sb.wtime,
            groups: ngroups,
            needs_recovery: sb.incompat & INCOMPAT_RECOVER != 0,
            path: path.to_path_buf(),
            sb,
            descs,
            all_used: false,
        })
    }

    /// Counts every block as used, for a filesystem whose journal needs recovery: blocks of the
    /// transactions still only in the journal look free in the bitmaps, and would be zeroed.
    /// The image then replays the journal at its first mount. The inodes cannot be trusted
    /// either: `inodes` fails from now on.
    pub fn assume_all_used(&mut self) {
        self.ranges = vec![(0, self.size)];
        self.all_used = true;
    }

    /// The device or file the filesystem is read from.
    pub fn device(&self) -> &Path {
        &self.path
    }

    /// Reads the inode tables (only the blocks holding inodes in use) and the mapping of every
    /// inode. Special inodes (journal, resize…) are left out: their blocks are always read.
    pub fn inodes(&self) -> Result<Inodes, Error> {
        if self.all_used {
            return Err(Error::Unsupported("journal not replayed: inode tables out of date".into()));
        }
        let sb = &self.sb;
        let bs = sb.block_size;
        let dev = Device::open(&self.path)?;
        let read = |buf: &mut [u8], offset: u64| dev.read_exact_at(buf, offset);
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
                if inode_bitmap >= sb.blocks || table + itable_blocks > sb.blocks {
                    return Err(Error::Format(format!("group {g}: inode bitmap at {inode_bitmap}, table at {table}")));
                }
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
                    let o = ((i % per_block) * sb.inode_size) as usize;
                    let raw = &block[o..o + sb.inode_size as usize];
                    if ino == sb.journal_inum {
                        if let Some((_, owned, _)) = inode(raw, sb, &read, true) {
                            out.journal = owned.into_iter().map(|(s, l)| (s * bs, l * bs)).collect();
                        }
                        continue;
                    }
                    // Reserved inodes (resize, bad blocks, quota, …) are never compared: their
                    // blocks are always read.
                    if (ino as u64) < sb.first_ino && ino != 2 {
                        continue;
                    }
                    if let Some((sig, owned, data)) = inode(raw, sb, &read, false) {
                        out.sigs.insert(ino, sig);
                        // Orphan file and quota files: metadata, written by the kernel without
                        // new times.
                        if !sb.internal.contains(&ino) {
                            out.file_data.extend(data.into_iter().map(|(s, l)| (s * bs, l * bs)));
                            out.owned.extend(owned.into_iter().map(|(s, l)| (s * bs, l * bs, ino)));
                        }
                    }
                }
            }
        }
        out.owned.sort_unstable();
        out.file_data.sort_unstable();
        Ok(out)
    }

    /// Byte ranges to capture while the filesystem is frozen, so that the image is consistent
    /// even if the rest is read with the filesystem in use: everything in use except the data of
    /// regular files, the inode table blocks without inodes in use and the journal past its
    /// superblock (empty once frozen).
    pub fn metadata_ranges(&self, inodes: &Inodes) -> Vec<(u64, u64)> {
        let mut skip: Vec<(u64, u64)> = inodes.file_data.clone();
        skip.extend(inodes.free_itable.iter().map(|&(s, l, _, _)| (s, l)));
        let mut journal = inodes.journal.clone();
        journal.sort_unstable();
        if let Some(first) = journal.first_mut() {
            // Keep the journal superblock.
            first.0 += self.block_size;
            first.1 -= self.block_size;
        }
        skip.extend(journal.into_iter().filter(|r| r.1 > 0));
        skip.sort_unstable();
        let mut out = Vec::new();
        let mut j = 0;
        for &(s, l) in &self.ranges {
            let mut p = s;
            let end = s + l;
            while p < end {
                while j < skip.len() && skip[j].0 + skip[j].1 <= p {
                    j += 1;
                }
                match skip.get(j) {
                    Some(&(ks, kl)) if ks <= p => p = (ks + kl).min(end),
                    Some(&(ks, _)) if ks < end => {
                        out.push((p, ks - p));
                        p = ks;
                    }
                    _ => {
                        out.push((p, end - p));
                        p = end;
                    }
                }
            }
        }
        out
    }

    /// Reads byte ranges of the filesystem.
    pub fn read_ranges(&self, ranges: &[(u64, u64)]) -> Result<Vec<(u64, Vec<u8>)>, Error> {
        let dev = Device::open(&self.path)?;
        ranges
            .iter()
            .map(|&(s, l)| {
                let mut buf = vec![0u8; l as usize];
                dev.read_exact_at(&mut buf, s)?;
                Ok((s, buf))
            })
            .collect()
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

/// Reads bytes of the device at an offset.
type Read<'a> = dyn Fn(&mut [u8], u64) -> std::io::Result<()> + 'a;

/// Running hash of an inode's block mapping. Saved between backups inside the signatures: it
/// must stay the same across versions (splitmix64 steps, no std hasher).
struct MapHash(u64);

impl MapHash {
    fn add(&mut self, v: u64) {
        let mut z = (self.0.rotate_left(23) ^ v).wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        self.0 = z ^ (z >> 31);
    }

    fn bytes(&mut self, b: &[u8]) {
        self.add(b.len() as u64);
        let (words, rest) = b.as_chunks::<8>();
        for w in words {
            self.add(u64::from_le_bytes(*w));
        }
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        self.add(u64::from_le_bytes(last));
    }
}

/// Signature, owned blocks and data blocks (first block, count) of an inode.
type InodeMap = ([u64; 4], Vec<(u64, u64)>, Vec<(u64, u64)>);

/// Signature and owned blocks (first block, count) of an inode in use; `None` if it cannot be
/// read with confidence (its blocks are then always read).
fn inode(raw: &[u8], sb: &Super, read: &Read, journal: bool) -> Option<InodeMap> {
    let mode = u16_at(raw, 0) as u32;
    if mode == 0 || u16_at(raw, 0x1A) == 0 {
        return None;
    }
    let extra = if raw.len() > 0x80 { u16_at(raw, 0x80) as usize } else { 0 };
    let field = |o: usize| if extra >= o + 4 - 0x80 && raw.len() >= o + 4 { u32_at(raw, o) as u64 } else { 0 };
    let flags = u32_at(raw, 0x20);
    let size = u32_at(raw, 4) as u64 | (u32_at(raw, 0x6C) as u64) << 32;
    let iblock = &raw[0x28..0x28 + 60];
    // mtime, then the mapping: i_block (extent root or block pointers) and every mapping block.
    let mut map = MapHash((u32_at(raw, 0x10) as u64) << 32 | field(0x88));
    map.bytes(iblock);
    let kind = mode & 0xF000;
    let mut owned = Vec::new();
    // Device files, FIFOs and sockets own no block; neither do inline data and fast symlinks.
    let blocks = u32_at(raw, 0x1C) != 0 || u16_at(raw, 0x74) != 0;
    let has_blocks = (journal || matches!(kind, 0x8000 | 0x4000 | 0xA000)) && flags & INODE_INLINE_DATA == 0 && blocks;
    let mut data = Vec::new();
    if has_blocks {
        let mut leaves = Vec::new();
        let ok = if flags & INODE_EXTENTS != 0 {
            extents(iblock, sb, read, &mut owned, &mut leaves, &mut map, 0)
        } else {
            indirect(iblock, sb, read, &mut owned, &mut leaves, &mut map)
        };
        if !ok {
            return None;
        }
        if journal {
            return Some(([0; 4], leaves, Vec::new()));
        }
        // Data blocks of regular files; those of directories and symlinks are metadata.
        if kind == 0x8000 {
            data = leaves.clone();
        }
        owned.extend(leaves);
    }
    let sig = [(u32_at(raw, 0xC) as u64) << 32 | field(0x84), map.0, size, (u32_at(raw, 0x64) as u64) << 32 | flags as u64];
    Some((sig, owned, data))
}

fn extents(node: &[u8], sb: &Super, read: &Read, owned: &mut Vec<(u64, u64)>, leaves: &mut Vec<(u64, u64)>, map: &mut MapHash, level: u32) -> bool {
    if u16_at(node, 0) != EXTENT_MAGIC || level > 5 {
        return false;
    }
    let (entries, depth) = (u16_at(node, 2) as usize, u16_at(node, 6));
    if 12 + entries * 12 > node.len() {
        return false;
    }
    if level > 0 {
        map.bytes(&node[..12 + entries * 12]);
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
            leaves.push((start, len));
        } else {
            let child = (u16_at(x, 8) as u64) << 32 | u32_at(x, 4) as u64;
            if child >= sb.blocks {
                return false;
            }
            let mut buf = vec![0u8; sb.block_size as usize];
            if read(&mut buf, child * sb.block_size).is_err() {
                return false;
            }
            owned.push((child, 1));
            if !extents(&buf, sb, read, owned, leaves, map, level + 1) {
                return false;
            }
        }
    }
    true
}

/// ext2/3 block map: 12 direct blocks, then single, double and triple indirect blocks.
fn indirect(iblock: &[u8], sb: &Super, read: &Read, owned: &mut Vec<(u64, u64)>, leaves: &mut Vec<(u64, u64)>, map: &mut MapHash) -> bool {
    fn add(v: &mut Vec<(u64, u64)>, b: u64) {
        match v.last_mut() {
            Some((s, l)) if *s + *l == b => *l += 1,
            _ => v.push((b, 1)),
        }
    }
    fn walk(b: u64, depth: u32, sb: &Super, read: &Read, owned: &mut Vec<(u64, u64)>, leaves: &mut Vec<(u64, u64)>, map: &mut MapHash) -> bool {
        if b == 0 {
            return true;
        }
        if b >= sb.blocks {
            return false;
        }
        if depth == 0 {
            add(leaves, b);
            return true;
        }
        add(owned, b);
        let mut buf = vec![0u8; sb.block_size as usize];
        if read(&mut buf, b * sb.block_size).is_err() {
            return false;
        }
        map.bytes(&buf);
        (0..buf.len() / 4).all(|i| walk(u32_at(&buf, i * 4) as u64, depth - 1, sb, read, owned, leaves, map))
    }
    (0..15).all(|i| walk(u32_at(iblock, i * 4) as u64, i.saturating_sub(11) as u32, sb, read, owned, leaves, map))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Superblock of a small ext4: 4 KiB blocks, 256-byte inodes.
    fn sb_buf() -> Vec<u8> {
        let mut b = vec![0u8; 1024];
        b[0x38..0x3A].copy_from_slice(&MAGIC.to_le_bytes());
        put32(&mut b, 0x4, 32768);
        put32(&mut b, 0x18, 2);
        put32(&mut b, 0x20, 32768);
        put32(&mut b, 0x28, 8192);
        put32(&mut b, 0x4C, 1);
        put32(&mut b, 0x54, 11);
        b[0x58..0x5A].copy_from_slice(&256u16.to_le_bytes());
        put32(&mut b, 0x60, INCOMPAT_FILETYPE | INCOMPAT_EXTENTS);
        b
    }

    /// Regular file with one extent of `len` blocks at `start`, in the inode itself.
    fn file_inode(start: u32, len: u16) -> Vec<u8> {
        let mut r = vec![0u8; 256];
        r[0..2].copy_from_slice(&0x81A4u16.to_le_bytes());
        put32(&mut r, 4, len as u32 * 4096);
        put32(&mut r, 0xC, 1_700_000_000);
        put32(&mut r, 0x10, 1_700_000_000);
        r[0x1A..0x1C].copy_from_slice(&1u16.to_le_bytes());
        put32(&mut r, 0x1C, len as u32 * 8);
        put32(&mut r, 0x20, INODE_EXTENTS);
        let e = &mut r[0x28..0x28 + 60];
        e[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        e[2..4].copy_from_slice(&1u16.to_le_bytes());
        e[4..6].copy_from_slice(&4u16.to_le_bytes());
        e[16..18].copy_from_slice(&len.to_le_bytes());
        put32(e, 20, start);
        r
    }

    fn no_read(_: &mut [u8], _: u64) -> std::io::Result<()> {
        Err(std::io::Error::other("no device"))
    }

    #[test]
    fn signature_follows_the_block_mapping() {
        let sb = Super::parse(&sb_buf()).unwrap();
        let (a, owned, data) = inode(&file_inode(1000, 4), &sb, &no_read, false).unwrap();
        assert_eq!(owned, vec![(1000, 4)]);
        assert_eq!(data, vec![(1000, 4)]);
        let (same, _, _) = inode(&file_inode(1000, 4), &sb, &no_read, false).unwrap();
        assert_eq!(a, same);
        // Same times, size and flags, blocks moved (e4defrag, resize2fs): another signature.
        let (moved, _, _) = inode(&file_inode(2000, 4), &sb, &no_read, false).unwrap();
        assert_eq!((a[0], a[2], a[3]), (moved[0], moved[2], moved[3]));
        assert_ne!(a, moved);
    }

    #[test]
    fn unsupported_features_refused() {
        let with = |o: usize, v: u32| {
            let mut b = sb_buf();
            put32(&mut b, o, v);
            Super::parse(&b)
        };
        assert!(Super::parse(&sb_buf()).is_ok());
        assert!(matches!(with(0x60, INCOMPAT_FILETYPE | INCOMPAT_JOURNAL_DEV), Err(Error::Unsupported(_))));
        assert!(matches!(with(0x60, INCOMPAT_FILETYPE | 0x800), Err(Error::Unsupported(_))));
        assert!(matches!(with(0x60, INCOMPAT_FILETYPE | 0x100_0000), Err(Error::Unsupported(_))));
        assert!(matches!(with(0x64, RO_COMPAT_BIGALLOC), Err(Error::Unsupported(_))));
        // A journal to replay is reported, not refused.
        assert!(with(0x60, INCOMPAT_FILETYPE | INCOMPAT_RECOVER).is_ok());
    }

    #[test]
    fn bad_geometry_is_an_error() {
        let with = |o: usize, v: u32| {
            let mut b = sb_buf();
            put32(&mut b, o, v);
            Super::parse(&b)
        };
        let mut b = sb_buf();
        b[0x58..0x5A].copy_from_slice(&0u16.to_le_bytes());
        assert!(Super::parse(&b).is_err(), "inode size 0");
        assert!(with(0x28, 0).is_err(), "no inodes per group");
        assert!(with(0x28, 8 * 4096 + 1).is_err(), "inode bitmap bigger than a block");
        assert!(with(0x14, 40000).is_err(), "first data block past the end");
        assert!(with(0x4, 0).is_err(), "no blocks");
        assert!(with(0x54, 3).is_err(), "first inode among the reserved ones");
        assert!(Super::parse(&[0u8; 100]).is_err(), "short buffer");
    }

    #[test]
    fn random_superblocks_do_not_panic() {
        let mut x = 0x1234_5678_9abc_def0u64;
        for _ in 0..20000 {
            let mut b = sb_buf();
            for _ in 0..4 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let o = (x % 0x290) as usize & !3;
                put32(&mut b, o, (x >> 32) as u32);
            }
            if let Ok(s) = Super::parse(&b) {
                let groups = s.groups();
                for g in [0, 1, groups / 2, groups.saturating_sub(1)] {
                    let _ = s.base_meta_blocks(g);
                    let _ = s.descriptor_block(g / s.descs_per_block());
                }
            }
        }
    }

    #[test]
    fn meta_bg_descriptors_before_first_meta_bg() {
        let mut b = sb_buf();
        put32(&mut b, 0x4, 32768 * 300);
        put32(&mut b, 0x60, INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_META_BG);
        put32(&mut b, 0x104, 1);
        put32(&mut b, 0x64, RO_COMPAT_SPARSE_SUPER);
        let s = Super::parse(&b).unwrap();
        // Group 0: superblock and the one old-style descriptor block, not all 3 of them.
        assert_eq!(s.base_meta_blocks(0), vec![0, 1]);
        // Group 128, first of the second meta group: its descriptor block, no superblock.
        assert_eq!(s.base_meta_blocks(128), vec![128 * 32768]);
    }

    /// An ext4 made by mkfs.ext4 in a temporary file (skipped if mkfs.ext4 is missing).
    fn mkfs(name: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("ext-image-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("fs.img");
        std::fs::File::create(&img).unwrap().set_len(16 << 20).unwrap();
        let ok = std::process::Command::new("mkfs.ext4").args(["-q", "-F", "-b", "4096"]).arg(&img).status().is_ok_and(|s| s.success());
        if !ok {
            eprintln!("mkfs.ext4 not available: skipped");
            return None;
        }
        Some(img)
    }

    #[test]
    fn journal_to_replay_counts_everything_as_used() {
        let Some(img) = mkfs("recover") else { return };
        let fs = ExtFs::open(&img).unwrap();
        assert!(!fs.needs_recovery);
        assert!(fs.used_bytes() < fs.size);
        let inodes = fs.inodes().unwrap();
        assert!(inodes.sigs.contains_key(&2));
        assert!(!inodes.journal.is_empty());
        // Mark the journal as needing recovery, as after a crash.
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
        use std::os::unix::fs::FileExt;
        let mut w = [0u8; 4];
        f.read_exact_at(&mut w, 1024 + 0x60).unwrap();
        f.write_all_at(&(u32::from_le_bytes(w) | INCOMPAT_RECOVER).to_le_bytes(), 1024 + 0x60).unwrap();
        let mut fs = ExtFs::open(&img).unwrap();
        assert!(fs.needs_recovery);
        fs.assume_all_used();
        assert_eq!(fs.ranges, vec![(0, fs.size)]);
        assert!(fs.inodes().is_err());
        let mut buf = vec![1u8; 4096];
        assert!(fs.fill_window(fs.size - 4096, &mut buf));
        assert!(buf.iter().all(|&b| b == 1), "nothing zeroed");
        // A device shorter than the filesystem.
        f.set_len(8 << 20).unwrap();
        assert!(matches!(ExtFs::open(&img), Err(Error::Format(_))));
        let _ = std::fs::remove_dir_all(img.parent().unwrap());
    }
}
