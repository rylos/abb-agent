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
/// All profiles: RAID0, RAID1, DUP, RAID10, RAID5, RAID6, RAID1C3, RAID1C4. On a single
/// device only single (none of them) and DUP are allowed.
pub const BLOCK_GROUP_PROFILE_MASK: u64 = 0b111_1111_1000;

pub const INCOMPAT_METADATA_UUID: u64 = 1 << 10;
/// Incompatible features that do not change what is read here: mixed_backref,
/// default_subvol, mixed_groups, compress_lzo, compress_zstd, big_metadata, extended_iref,
/// raid56 and raid1c34 (the profiles themselves are refused chunk by chunk), skinny_metadata,
/// no_holes, metadata_uuid, simple_quota. Refused: zoned, extent_tree_v2, raid_stripe_tree and
/// anything newer, whose trees or allocation this reader does not know.
pub const INCOMPAT_SUPPORTED: u64 = 0xfff | 1 << 16;
pub const CSUM_TYPE_CRC32C: u16 = 0;
/// Size of the superblock's copy of the system chunks.
const SYS_CHUNK_ARRAY_MAX: u32 = 2048;

/// Size of the oldest root item format; newer ones add fields after it (uuids, otransid…).
pub const ROOT_ITEM_MIN: usize = 239;

/// Root node of a tree from its ROOT_ITEM: (bytenr, generation).
pub fn root_location(item: &[u8]) -> Result<(u64, u64), Error> {
    if item.len() < ROOT_ITEM_MIN {
        return Err(Error::Format(format!("root item of {} bytes", item.len())));
    }
    Ok((u64_at(item, 176), u64_at(item, 160)))
}

/// A ROOT_REF or ROOT_BACKREF item: (directory inode, index in it, name).
pub fn root_ref(item: &[u8]) -> Result<(u64, u64, &[u8]), Error> {
    let name = item.get(16..18).map(|l| u16::from_le_bytes([l[0], l[1]]) as usize).and_then(|l| item.get(18..18 + l));
    match name {
        Some(name) => Ok((u64_at(item, 0), u64_at(item, 8), name)),
        None => Err(Error::Format("truncated root ref".into())),
    }
}

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
        let unknown = sb.incompat_flags & !INCOMPAT_SUPPORTED;
        if unknown != 0 {
            return Err(Error::Unsupported(format!("incompatible features 0x{unknown:x}")));
        }
        // The kernel's checks (btrfs_validate_super) on what sizes and indexes everything else.
        let (sector, node) = (sb.sectorsize, sb.nodesize);
        if !sector.is_power_of_two() || !(512..=65536).contains(&sector) {
            return Err(Error::Format(format!("sector size {sector}")));
        }
        if !node.is_power_of_two() || node < sector || node > 65536 || (node as usize) < HEADER_SIZE + KEY_PTR_SIZE {
            return Err(Error::Format(format!("node size {node}")));
        }
        if sb.sys_chunk_array_size > SYS_CHUNK_ARRAY_MAX {
            return Err(Error::Format(format!("sys_chunk_array_size {}", sb.sys_chunk_array_size)));
        }
        if sb.total_bytes == 0 || sb.root_level > 7 || sb.chunk_root_level > 7 {
            return Err(Error::Format("invalid superblock".into()));
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
        if b.len() < 48 {
            return Err(Error::Format("truncated chunk item".into()));
        }
        let length = u64_at(b, 0);
        let kind = u64_at(b, 24);
        let num_stripes = u16_at(b, 44) as usize;
        let size = 48 + num_stripes * 32;
        if b.len() < size {
            return Err(Error::Format("truncated chunk item".into()));
        }
        if num_stripes == 0 || length == 0 || logical.checked_add(length).is_none() {
            return Err(Error::Format(format!("chunk at {logical}: {num_stripes} stripes, length {length}")));
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
        if num_stripes != if profile == BLOCK_GROUP_DUP { 2 } else { 1 } {
            return Err(Error::Format(format!("chunk at {logical}: {num_stripes} stripes for profile 0x{profile:x}")));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn put(b: &mut [u8], o: usize, v: &[u8]) {
        b[o..o + v.len()].copy_from_slice(v);
    }

    fn sb_raw(f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut b = vec![0u8; SUPER_SIZE];
        put(&mut b, 0x40, MAGIC);
        put(&mut b, 0x70, &(1u64 << 30).to_le_bytes());
        put(&mut b, 0x88, &1u64.to_le_bytes());
        put(&mut b, 0x90, &4096u32.to_le_bytes());
        put(&mut b, 0x94, &16384u32.to_le_bytes());
        put(&mut b, 0xbc, &(1u64 | 1 << 8).to_le_bytes());
        f(&mut b);
        let c = csum(&b[32..]);
        put(&mut b, 0, &c);
        b
    }

    #[test]
    fn superblock_features_and_sizes() {
        assert!(Superblock::parse(sb_raw(|_| {})).is_ok());
        let flags = |v: u64| sb_raw(|b| put(b, 0xbc, &v.to_le_bytes()));
        // zoned, extent_tree_v2, raid_stripe_tree, unknown.
        for bit in [12, 13, 14, 15, 17, 40] {
            assert!(matches!(Superblock::parse(flags(1 | 1 << bit)), Err(Error::Unsupported(_))), "bit {bit}");
        }
        assert!(Superblock::parse(flags(0xfff | 1 << 16)).is_ok());
        let size = |o: usize, v: u32| sb_raw(|b| put(b, o, &v.to_le_bytes()));
        for (o, v) in [(0x94, 0), (0x94, 3000), (0x94, 2048), (0x94, 1 << 17), (0x90, 0), (0x90, 6000), (0xa0, 2049)] {
            assert!(matches!(Superblock::parse(size(o, v)), Err(Error::Format(_))), "0x{o:x} = {v}");
        }
        let sys = |n: u32| Superblock::parse(sb_raw(|b| put(b, 0xa0, &n.to_le_bytes()))).unwrap().sys_chunk_array().len();
        assert_eq!(sys(2048), 2048);
    }

    fn chunk(kind: u64, stripes: &[u64]) -> Vec<u8> {
        let mut b = vec![0u8; 48 + 32 * stripes.len()];
        put(&mut b, 0, &(1u64 << 20).to_le_bytes());
        put(&mut b, 24, &kind.to_le_bytes());
        put(&mut b, 44, &(stripes.len() as u16).to_le_bytes());
        for (i, s) in stripes.iter().enumerate() {
            put(&mut b, 48 + 32 * i, &1u64.to_le_bytes());
            put(&mut b, 56 + 32 * i, &s.to_le_bytes());
        }
        b
    }

    #[test]
    fn chunk_profiles_and_stripes() {
        let (c, size) = Chunk::parse(1 << 20, &chunk(BLOCK_GROUP_METADATA | BLOCK_GROUP_DUP, &[1 << 20, 2 << 20]), 1).unwrap();
        assert_eq!((c.stripes.len(), size), (2, 112));
        assert!(Chunk::parse(0, &chunk(BLOCK_GROUP_DATA, &[1 << 20]), 1).is_ok());
        // RAID1C4 (bit 10) and RAID1C3 (bit 9), RAID1.
        for p in [1 << 10, 1 << 9, 1 << 4] {
            assert!(matches!(Chunk::parse(0, &chunk(BLOCK_GROUP_DATA | p, &[0, 1 << 20]), 1), Err(Error::Unsupported(_))));
        }
        assert!(matches!(Chunk::parse(0, &chunk(BLOCK_GROUP_DATA, &[]), 1), Err(Error::Format(_))));
        assert!(matches!(Chunk::parse(0, &chunk(BLOCK_GROUP_DATA | BLOCK_GROUP_DUP, &[0]), 1), Err(Error::Format(_))));
        assert!(matches!(Chunk::parse(0, &[0u8; 30], 1), Err(Error::Format(_))));
        assert!(matches!(Chunk::parse(0, &chunk(BLOCK_GROUP_DATA, &[0])[..60], 1), Err(Error::Format(_))));
    }

    #[test]
    fn root_refs_bounded() {
        let mut r = vec![0u8; 18];
        put(&mut r, 16, &5u16.to_le_bytes());
        assert!(root_ref(&r).is_err());
        r.extend(b"@home");
        assert_eq!(root_ref(&r).unwrap().2, b"@home");
        assert!(root_ref(&r[..10]).is_err());
        assert!(root_location(&[0u8; 200]).is_err());
    }

    #[test]
    fn random_chunk_items_do_not_panic() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..20000 {
            let mut b = chunk(BLOCK_GROUP_DUP, &[0, 1 << 20]);
            for _ in 0..3 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let o = (x % b.len() as u64) as usize;
                b[o] = (x >> 40) as u8;
            }
            let cut = (x >> 50) as usize % (b.len() + 1);
            let _ = Chunk::parse(0, &b[..cut], 1);
        }
    }
}
