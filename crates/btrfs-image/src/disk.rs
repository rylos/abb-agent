//! btrfs on-disk structures (little-endian), only the fields we need.
//! Reference: fs/btrfs/accessors.h and include/uapi/linux/btrfs_tree.h in the kernel.

use crate::Error;

pub const SUPER_OFFSET: u64 = 0x1_0000;
pub const SUPER_SIZE: usize = 4096;
/// Superblock copies: 64 KiB, 64 MiB, 256 GiB.
pub const SUPER_MIRRORS: [u64; 3] = [0x1_0000, 0x400_0000, 0x40_0000_0000];
pub const MAGIC: &[u8; 8] = b"_BHRfS_M";

pub const HEADER_SIZE: usize = 101;
pub const ITEM_SIZE: usize = 25;
pub const KEY_PTR_SIZE: usize = 33;

// Key types
pub const ROOT_ITEM_KEY: u8 = 132;
pub const EXTENT_ITEM_KEY: u8 = 168;
pub const METADATA_ITEM_KEY: u8 = 169;
pub const CHUNK_ITEM_KEY: u8 = 228;

// Trees
pub const ROOT_TREE_OBJECTID: u64 = 1;
pub const EXTENT_TREE_OBJECTID: u64 = 2;
pub const CHUNK_TREE_OBJECTID: u64 = 3;

pub const EXTENT_FLAG_TREE_BLOCK: u64 = 2;

// Chunk types / profiles
pub const BLOCK_GROUP_DATA: u64 = 1 << 0;
pub const BLOCK_GROUP_SYSTEM: u64 = 1 << 1;
pub const BLOCK_GROUP_METADATA: u64 = 1 << 2;
pub const BLOCK_GROUP_DUP: u64 = 1 << 5;
/// All RAID profiles: on a single device only single and DUP are allowed.
pub const BLOCK_GROUP_PROFILE_MASK: u64 = 0b11_1101_1000;

pub const INCOMPAT_METADATA_UUID: u64 = 1 << 10;
pub const CSUM_TYPE_CRC32C: u16 = 0;

pub fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
pub fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// btrfs crc32c of `data`, in its on-disk format (first 4 bytes of the csum field).
pub fn csum(data: &[u8]) -> [u8; 4] {
    crc32c::crc32c(data).to_le_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub objectid: u64,
    pub kind: u8,
    pub offset: u64,
}

impl Key {
    pub fn parse(b: &[u8]) -> Key {
        Key { objectid: u64_at(b, 0), kind: b[8], offset: u64_at(b, 9) }
    }
}

#[derive(Clone, Debug)]
pub struct Superblock {
    pub raw: Vec<u8>,
    pub fsid: [u8; 16],
    pub metadata_uuid: [u8; 16],
    pub bytenr: u64,
    pub generation: u64,
    pub root: u64,
    pub chunk_root: u64,
    pub log_root: u64,
    pub total_bytes: u64,
    pub bytes_used: u64,
    pub num_devices: u64,
    pub sectorsize: u32,
    pub nodesize: u32,
    pub sys_chunk_array_size: u32,
    pub chunk_root_generation: u64,
    pub incompat_flags: u64,
    pub csum_type: u16,
    pub root_level: u8,
    pub chunk_root_level: u8,
    pub log_root_level: u8,
    pub devid: u64,
}

impl Superblock {
    pub fn parse(raw: Vec<u8>) -> Result<Superblock, Error> {
        if raw.len() != SUPER_SIZE || &raw[0x40..0x48] != MAGIC {
            return Err(Error::Format("superblock magic missing".into()));
        }
        if raw[..4] != csum(&raw[32..]) {
            return Err(Error::Format("bad superblock checksum".into()));
        }
        let fsid: [u8; 16] = raw[0x20..0x30].try_into().unwrap();
        let incompat_flags = u64_at(&raw, 0xbc);
        let metadata_uuid = if incompat_flags & INCOMPAT_METADATA_UUID != 0 {
            raw[0x23b..0x24b].try_into().unwrap()
        } else {
            fsid
        };
        let sb = Superblock {
            fsid,
            metadata_uuid,
            bytenr: u64_at(&raw, 0x30),
            generation: u64_at(&raw, 0x48),
            root: u64_at(&raw, 0x50),
            chunk_root: u64_at(&raw, 0x58),
            log_root: u64_at(&raw, 0x60),
            total_bytes: u64_at(&raw, 0x70),
            bytes_used: u64_at(&raw, 0x78),
            num_devices: u64_at(&raw, 0x88),
            sectorsize: u32_at(&raw, 0x90),
            nodesize: u32_at(&raw, 0x94),
            sys_chunk_array_size: u32_at(&raw, 0xa0),
            chunk_root_generation: u64_at(&raw, 0xa4),
            incompat_flags,
            csum_type: u16_at(&raw, 0xc4),
            root_level: raw[0xc6],
            chunk_root_level: raw[0xc7],
            log_root_level: raw[0xc8],
            devid: u64_at(&raw, 0xc9),
            raw,
        };
        if sb.csum_type != CSUM_TYPE_CRC32C {
            return Err(Error::Unsupported(format!("csum_type {} (only crc32c supported)", sb.csum_type)));
        }
        if sb.num_devices != 1 {
            return Err(Error::Unsupported(format!("{} devices in the filesystem (only 1 supported)", sb.num_devices)));
        }
        Ok(sb)
    }

    pub fn sys_chunk_array(&self) -> &[u8] {
        &self.raw[0x32b..0x32b + self.sys_chunk_array_size as usize]
    }

    /// Copy of the superblock ready to be written at `mirror_offset`, without the tree-log
    /// (the log of fsyncs after the commit is discarded) and with the checksum recomputed.
    pub fn without_log(&self, mirror_offset: u64) -> Vec<u8> {
        let mut raw = self.raw.clone();
        raw[0x30..0x38].copy_from_slice(&mirror_offset.to_le_bytes());
        raw[0x60..0x68].fill(0); // log_root
        raw[0xc8] = 0; // log_root_level
        let c = csum(&raw[32..]);
        raw[..32].fill(0);
        raw[..4].copy_from_slice(&c);
        raw
    }
}

#[derive(Clone, Debug)]
pub struct Chunk {
    pub logical: u64,
    pub length: u64,
    pub kind: u64,
    /// Physical offsets on the device, one per stripe (2 for DUP).
    pub stripes: Vec<u64>,
}

impl Chunk {
    /// `b` starts with the chunk item (after the key); returns the chunk and its length in bytes.
    pub fn parse(logical: u64, b: &[u8], devid: u64) -> Result<(Chunk, usize), Error> {
        let length = u64_at(b, 0);
        let kind = u64_at(b, 24);
        let num_stripes = u16_at(b, 44) as usize;
        let size = 48 + num_stripes * 32;
        if b.len() < size {
            return Err(Error::Format("truncated chunk item".into()));
        }
        let mut stripes = Vec::with_capacity(num_stripes);
        for i in 0..num_stripes {
            let s = &b[48 + i * 32..];
            if u64_at(s, 0) != devid {
                return Err(Error::Unsupported("chunk on another device".into()));
            }
            stripes.push(u64_at(s, 8));
        }
        let profile = kind & BLOCK_GROUP_PROFILE_MASK;
        if profile != 0 && profile != BLOCK_GROUP_DUP {
            return Err(Error::Unsupported(format!("chunk profile 0x{profile:x} (only single and DUP supported)")));
        }
        Ok((Chunk { logical, length, kind, stripes }, size))
    }
}

/// Header of a tree node.
#[derive(Clone, Copy, Debug)]
pub struct NodeHeader {
    pub bytenr: u64,
    pub generation: u64,
    pub owner: u64,
    pub nritems: u32,
    pub level: u8,
}

impl NodeHeader {
    pub fn parse(b: &[u8]) -> NodeHeader {
        NodeHeader {
            bytenr: u64_at(b, 48),
            generation: u64_at(b, 80),
            owner: u64_at(b, 88),
            nritems: u32_at(b, 96),
            level: b[100],
        }
    }
}
