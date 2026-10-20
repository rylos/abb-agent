//! Fingerprints for incrementals (CBT): one 64-bit value per 64 KiB block of the volume, which
//! changes if and only if the used content of the block changes.
//!
//! Data is not re-read: checksummed data uses the checksums from the csum tree; for metadata
//! nodes the address and allocation transaction are enough (a btrfs node is never modified in
//! place); only data without checksums (NOCOW files, preallocated extents never written) is
//! read and run through crc32c. This way in-place writes (NOCOW, preallocated) are detected
//! too, unlike a comparison based on generations alone.

use crate::capture::Capture;
use crate::disk::*;
use crate::usedmap::{Kind, UsedMap};
use crate::{Error, Filesystem};
use std::io::{Read, Write};
use std::path::Path;

pub const BLOCK: u64 = 64 * 1024;
/// Fingerprint of blocks that must always be resent (superblock, first MiB of the volume).
pub const ALWAYS: u64 = u64::MAX;

const CSUM_TREE_OBJECTID: u64 = 7;
const EXTENT_CSUM_OBJECTID: u64 = u64::MAX - 9;
const EXTENT_CSUM_KEY: u8 = 128;
const CSUM_SIZE: usize = 4;
const MAGIC: &[u8; 8] = b"ABBCBT01";
/// Tags the contributions of sectors without checksums, which are computed from the content.
const UNCOVERED: u64 = 1 << 40;

/// 0 = unused block.
pub struct Digests {
    pub table: Vec<u64>,
    /// Data sectors covered by checksums (bitset over physical sectors); empty for tables
    /// loaded from file, which are only used for comparison.
    covered: Vec<u64>,
    sector: u64,
}

fn mix(x: u64) -> u64 {
    // splitmix64
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Digests {
    fn add(&mut self, physical: u64, value: u64) {
        let i = (physical / BLOCK) as usize;
        if self.table[i] != ALWAYS {
            // Sum: independent of the order in which contributions arrive.
            self.table[i] = self.table[i].wrapping_add(mix(physical ^ mix(value)));
        }
    }

    pub fn compute(fs: &Filesystem, cap: &Capture, map: &UsedMap) -> Result<Digests, Error> {
        let sector = fs.sb.sectorsize as u64;
        let nsect = fs.sb.total_bytes.div_ceil(sector) as usize;
        let mut d = Digests {
            table: vec![0; fs.sb.total_bytes.div_ceil(BLOCK) as usize],
            covered: vec![0u64; nsect.div_ceil(64)],
            sector,
        };

        for r in &map.ranges {
            match r.kind {
                Kind::Reserved => {
                    for b in r.physical / BLOCK..=(r.physical + r.len - 1) / BLOCK {
                        d.table[b as usize] = ALWAYS;
                    }
                }
                Kind::Tree => d.add(r.physical, r.logical ^ mix(r.generation ^ r.salt)),
                Kind::Data => {}
            }
        }

        let (csum_root, csum_gen) = fs.tree_root(CSUM_TREE_OBJECTID)?;
        fs.walk(csum_root, csum_gen, &mut |_, key, data| {
            if key.objectid != EXTENT_CSUM_OBJECTID || key.kind != EXTENT_CSUM_KEY {
                return Ok(());
            }
            let mut chunk: Option<(u64, u64, Vec<u64>)> = None; // (start, end, stripes)
            for (k, c) in data.as_chunks::<CSUM_SIZE>().0.iter().enumerate() {
                let logical = key.offset + k as u64 * sector;
                // Checksums of data left out of the image do not count.
                let x = &map.excluded_data;
                let i = x.partition_point(|d| d.0 + d.1 <= logical);
                if x.get(i).is_some_and(|d| d.0 <= logical) {
                    continue;
                }
                if !chunk.as_ref().is_some_and(|(s, e, _)| (*s..*e).contains(&logical)) {
                    let ch = fs.chunk_of(logical)?;
                    chunk = Some((ch.logical, ch.logical + ch.length, ch.stripes.clone()));
                }
                let (start, _, stripes) = chunk.as_ref().unwrap();
                let csum = u32::from_le_bytes(*c) as u64;
                for st in stripes {
                    let phys = st + (logical - start);
                    d.add(phys, csum);
                    let s = (phys / sector) as usize;
                    d.covered[s / 64] |= 1 << (s % 64);
                }
            }
            Ok(())
        })?;

        // Data without checksums: crc32c of the content, sector by sector.
        let mut buf = Vec::new();
        for r in map.ranges.iter().filter(|r| r.kind == Kind::Data) {
            let first = r.physical / sector;
            let n = r.len / sector;
            let uncovered: Vec<u64> =
                (first..first + n).filter(|&s| !d.is_covered(s)).collect();
            if uncovered.is_empty() {
                continue;
            }
            let content: &[u8] = match cap.data.get(&r.logical) {
                Some(c) => c,
                None => {
                    buf.resize(r.len as usize, 0);
                    fs.read_at(&mut buf, r.physical)?;
                    &buf
                }
            };
            for s in uncovered {
                let o = ((s - first) * sector) as usize;
                let crc = crc32c::crc32c(&content[o..o + sector as usize]) as u64;
                d.add(s * sector, crc | UNCOVERED);
            }
        }

        // A used block must never have fingerprint 0.
        for r in &map.ranges {
            for b in r.physical / BLOCK..=(r.physical + r.len - 1) / BLOCK {
                if d.table[b as usize] == 0 {
                    d.table[b as usize] = 1;
                }
            }
        }
        Ok(d)
    }

    /// Table computed elsewhere (e.g. ext, from the data read while sending).
    pub fn from_table(table: Vec<u64>) -> Digests {
        Digests { table, covered: Vec::new(), sector: 0 }
    }

    fn is_covered(&self, s: u64) -> bool {
        self.covered[s as usize / 64] & (1 << (s % 64)) != 0
    }

    /// Re-checks the bytes about to be sent for block `block` (`data`, at offset
    /// `block * BLOCK` in the volume, `BLOCK` long or up to the end of the volume): recomputes
    /// the fingerprint from the data and compares it with the one derived from the btrfs
    /// checksums. This way every checksummed data sector is verified against the csum tree (a
    /// read error, a block corrupted on disk or changed under the snapshot does not end up in
    /// the backup). Metadata nodes are already verified by `fill_window`.
    pub fn verify_block(&self, map: &UsedMap, block: usize, data: &[u8]) -> Result<(), Error> {
        let expected = self.table[block];
        if self.covered.is_empty() || expected == ALWAYS || expected == 0 {
            return Ok(());
        }
        let start = block as u64 * BLOCK;
        let end = start + data.len() as u64;
        let mut sum = 0u64;
        let first = map.ranges.partition_point(|r| r.physical + r.len <= start.saturating_sub(128 << 20));
        for r in &map.ranges[first..] {
            if r.physical >= end {
                break;
            }
            match r.kind {
                Kind::Tree if r.physical / BLOCK == block as u64 => {
                    sum = sum.wrapping_add(mix(r.physical ^ mix(r.logical ^ mix(r.generation ^ r.salt))));
                }
                Kind::Data => {
                    let (rs, re) = (r.physical.max(start), (r.physical + r.len).min(end));
                    let mut p = rs;
                    while p < re {
                        let o = (p - start) as usize;
                        let crc = crc32c::crc32c(&data[o..o + self.sector as usize]) as u64;
                        let v = if self.is_covered(p / self.sector) { crc } else { crc | UNCOVERED };
                        sum = sum.wrapping_add(mix(p ^ mix(v)));
                        p += self.sector;
                    }
                }
                _ => {}
            }
        }
        if sum == 0 {
            sum = 1;
        }
        if sum != expected {
            return Err(Error::Format(format!(
                "data of the block at {start} does not match the btrfs checksums"
            )));
        }
        Ok(())
    }

    /// Explains a failed `verify_block`, for the log: each range of the block with its extent in
    /// the backed-up transaction and the trees referencing it there, and for every data sector
    /// whether it is covered by a checksum and whether the device still holds what was read.
    pub fn explain_block(&self, fs: &Filesystem, cap: &Capture, map: &UsedMap, block: usize, data: &[u8]) -> String {
        let start = block as u64 * BLOCK;
        let end = start + data.len() as u64;
        let mut out = format!("block at {start} ({} bytes), generation {}:", data.len(), fs.sb.generation);
        let mut now = vec![0u8; data.len()];
        let reread = fs.read_at(&mut now, start).is_ok();
        let first = map.ranges.partition_point(|r| r.physical + r.len <= start.saturating_sub(128 << 20));
        let ranges: Vec<_> = map.ranges[first..].iter().take_while(|r| r.physical < end).filter(|r| r.physical + r.len > start).collect();
        // Checksums in the backed-up transaction of the data sectors of the block, by physical sector.
        let mut expected = std::collections::HashMap::new();
        if let Ok((root, generation)) = fs.tree_root(CSUM_TREE_OBJECTID) {
            let _ = fs.walk(root, generation, &mut |_, key, item| {
                if key.objectid != EXTENT_CSUM_OBJECTID || key.kind != EXTENT_CSUM_KEY {
                    return Ok(());
                }
                for (k, c) in item.as_chunks::<CSUM_SIZE>().0.iter().enumerate() {
                    let logical = key.offset + k as u64 * self.sector;
                    for r in ranges.iter().filter(|r| r.kind == Kind::Data && logical >= r.logical && logical < r.logical + r.len) {
                        let p = r.physical + (logical - r.logical);
                        if p >= start && p < end {
                            expected.insert(p, u32::from_le_bytes(*c));
                        }
                    }
                }
                Ok(())
            });
        }
        let mut logicals = Vec::new();
        for r in ranges {
            out += &format!(
                "\n  {:?} physical {} logical {} len {} generation {}{}",
                r.kind,
                r.physical,
                r.logical,
                r.len,
                r.generation,
                if cap.data.contains_key(&r.logical) { " captured" } else { "" }
            );
            if r.kind != Kind::Data {
                continue;
            }
            logicals.push(r.logical);
            let (rs, re) = (r.physical.max(start), (r.physical + r.len).min(end));
            let mut p = rs;
            while p < re {
                let o = (p - start) as usize;
                let sent = crc32c::crc32c(&data[o..o + self.sector as usize]);
                let disk = crc32c::crc32c(&now[o..o + self.sector as usize]);
                out += &format!(
                    "\n    sector {p}: {} crc {sent:08x}{}",
                    match expected.get(&p) {
                        Some(&e) if e == sent => "csum ok".to_string(),
                        Some(&e) => format!("csum {e:08x} MISMATCH"),
                        None if self.is_covered(p / self.sector) => "csum not found".to_string(),
                        None => "no csum".to_string(),
                    },
                    if !reread { ", re-read failed".to_string() } else if disk != sent { format!(", now {disk:08x} on disk") } else { String::new() }
                );
                p += self.sector;
            }
        }
        // References of those extents in the backed-up transaction (inline and keyed).
        if let Ok((root, generation)) = fs.tree_root(EXTENT_TREE_OBJECTID) {
            let mut refs: Vec<String> = Vec::new();
            let _ = fs.walk(root, generation, &mut |_, key, item| {
                if !logicals.contains(&key.objectid) {
                    return Ok(());
                }
                match key.kind {
                    EXTENT_ITEM_KEY if item.len() >= 24 => {
                        refs.push(format!("{}: extent refs {} generation {} flags {}", key.objectid, u64_at(item, 0), u64_at(item, 8), u64_at(item, 16)));
                        let mut o = 24;
                        while o < item.len() {
                            let t = item[o];
                            o += 1;
                            match t {
                                178 if o + 28 <= item.len() => {
                                    refs.push(format!("{}:   root {} inode {} offset {} count {}", key.objectid, u64_at(item, o), u64_at(item, o + 8), u64_at(item, o + 16), u32::from_le_bytes(item[o + 24..o + 28].try_into().unwrap())));
                                    o += 28;
                                }
                                184 if o + 12 <= item.len() => {
                                    refs.push(format!("{}:   shared by leaf {}", key.objectid, u64_at(item, o)));
                                    o += 12;
                                }
                                172 | 176 | 182 if o + 8 <= item.len() => {
                                    refs.push(format!("{}:   ref type {t} {}", key.objectid, u64_at(item, o)));
                                    o += 8;
                                }
                                _ => break,
                            }
                        }
                    }
                    178 if item.len() >= 24 => refs.push(format!("{}:   root {} inode {} offset {}", key.objectid, u64_at(item, 0), u64_at(item, 8), u64_at(item, 16))),
                    184 => refs.push(format!("{}:   shared by leaf {}", key.objectid, key.offset)),
                    _ => {}
                }
                Ok(())
            });
            for r in refs {
                out += &format!("\n  {r}");
            }
        }
        out
    }

    pub fn used(&self, block: usize) -> bool {
        self.table.get(block).is_some_and(|&v| v != 0)
    }

    /// Whether the block must be resent relative to `prev`.
    pub fn changed(&self, prev: &Digests, block: usize) -> bool {
        let cur = self.table.get(block).copied().unwrap_or(0);
        cur == ALWAYS || prev.table.get(block).copied().unwrap_or(0) != cur
    }

    /// Saves the table together with the data that ties it to a server version.
    pub fn save(&self, path: &Path, meta: &CbtMeta) -> std::io::Result<()> {
        let raw: Vec<u8> = self.table.iter().flat_map(|v| v.to_le_bytes()).collect();
        let packed = lz4_flex::block::compress_prepend_size(&raw);
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(MAGIC)?;
        for v in [meta.version_id, meta.generation, meta.partition_offset, meta.table_len()] {
            f.write_all(&v.to_le_bytes())?;
        }
        f.write_all(&packed)?;
        f.sync_all()?;
        std::fs::rename(tmp, path)
    }

    pub fn load(path: &Path) -> std::io::Result<(Digests, CbtMeta)> {
        let mut f = std::fs::File::open(path)?;
        let mut all = Vec::new();
        f.read_to_end(&mut all)?;
        if all.len() < 40 || &all[..8] != MAGIC {
            return Err(std::io::Error::other("invalid CBT file"));
        }
        let meta = CbtMeta {
            version_id: u64_at(&all, 8),
            generation: u64_at(&all, 16),
            partition_offset: u64_at(&all, 24),
            len: u64_at(&all, 32),
        };
        let raw = lz4_flex::block::decompress_size_prepended(&all[40..]).map_err(std::io::Error::other)?;
        let table: Vec<u64> = raw.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect();
        if table.len() as u64 != meta.len {
            return Err(std::io::Error::other("truncated CBT file"));
        }
        Ok((Digests { table, covered: Vec::new(), sector: 0 }, meta))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CbtMeta {
    /// Server version the table corresponds to.
    pub version_id: u64,
    pub generation: u64,
    pub partition_offset: u64,
    len: u64,
}

impl CbtMeta {
    pub fn new(version_id: u64, generation: u64, partition_offset: u64, d: &Digests) -> CbtMeta {
        CbtMeta { version_id, generation, partition_offset, len: d.table.len() as u64 }
    }

    fn table_len(&self) -> u64 {
        self.len
    }
}
