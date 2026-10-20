//! GUID partition tables: reading, and writing them back for a disk of a different size or
//! with new partitions.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

pub const ESP: [u8; 16] = guid_bytes(0xC12A7328, 0xF81F, 0x11D2, [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
pub const BIOS_BOOT: [u8; 16] = guid_bytes(0x21686148, 0x6449, 0x6E6F, [0x74, 0x4E, 0x65, 0x65, 0x64, 0x45, 0x46, 0x49]);
pub const LINUX_FS: [u8; 16] = guid_bytes(0x0FC63DAF, 0x8483, 0x4772, [0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);

const fn guid_bytes(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let (b, c) = (b.to_le_bytes(), c.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

pub fn random_guid() -> [u8; 16] {
    let mut g = [0u8; 16];
    if let Ok(f) = File::open("/dev/urandom") {
        let _ = f.read_exact_at(&mut g, 0);
    }
    g[7] = (g[7] & 0x0f) | 0x40;
    g[8] = (g[8] & 0x3f) | 0x80;
    g
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub type_guid: [u8; 16],
    pub guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    /// UTF-16LE, 72 bytes.
    pub name: Vec<u8>,
}

impl Entry {
    pub fn is_used(&self) -> bool {
        self.type_guid != [0; 16]
    }

    #[allow(dead_code)]
    pub fn name(&self) -> String {
        let units: Vec<u16> = self.name.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        String::from_utf16_lossy(&units)
    }
}

#[derive(Clone, Debug)]
pub struct Gpt {
    pub sector: u64,
    pub disk_guid: [u8; 16],
    /// All slots, used or not, in order (partition N is `entries[N-1]`).
    pub entries: Vec<Entry>,
    pub entry_size: usize,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

impl Gpt {
    /// Parses the primary GPT from the start of a disk (`buf` from byte 0, at least up to the end
    /// of the entry array). Returns None if there is no valid GPT.
    pub fn parse(buf: &[u8], sector: u64) -> Option<Gpt> {
        let s = sector as usize;
        let h = buf.get(s..s + 92)?;
        if &h[0..8] != b"EFI PART" {
            return None;
        }
        let hsize = u32_at(h, 12) as usize;
        let mut hdr = buf.get(s..s + hsize)?.to_vec();
        let crc = u32_at(&hdr, 16);
        hdr[16..20].fill(0);
        if crc32(&hdr) != crc {
            return None;
        }
        let (start, count, size) = (u64_at(h, 72) as usize * s, u32_at(h, 80) as usize, u32_at(h, 84) as usize);
        let array = buf.get(start..start + count * size)?;
        if crc32(array) != u32_at(h, 88) {
            return None;
        }
        let entries = array
            .chunks(size)
            .map(|e| Entry {
                type_guid: e[0..16].try_into().unwrap(),
                guid: e[16..32].try_into().unwrap(),
                first_lba: u64_at(e, 32),
                last_lba: u64_at(e, 40),
                attributes: u64_at(e, 48),
                name: e[56..128].to_vec(),
            })
            .collect();
        Some(Gpt { sector, disk_guid: h[56..72].try_into().unwrap(), entries, entry_size: size })
    }

    /// Reads the primary GPT of a device.
    pub fn read(dev: &File, sector: u64) -> io::Result<Option<Gpt>> {
        let mut buf = vec![0u8; (34 * sector).max(64 * 1024) as usize];
        dev.read_exact_at(&mut buf, 0)?;
        Ok(Gpt::parse(&buf, sector))
    }

    pub fn used(&self) -> impl Iterator<Item = (u64, &Entry)> {
        self.entries.iter().enumerate().filter(|(_, e)| e.is_used()).map(|(i, e)| (i as u64 + 1, e))
    }

    fn array(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.entries.len() * self.entry_size];
        for (i, e) in self.entries.iter().enumerate() {
            let o = i * self.entry_size;
            out[o..o + 16].copy_from_slice(&e.type_guid);
            out[o + 16..o + 32].copy_from_slice(&e.guid);
            out[o + 32..o + 40].copy_from_slice(&e.first_lba.to_le_bytes());
            out[o + 40..o + 48].copy_from_slice(&e.last_lba.to_le_bytes());
            out[o + 48..o + 56].copy_from_slice(&e.attributes.to_le_bytes());
            let n = e.name.len().min(72);
            out[o + 56..o + 56 + n].copy_from_slice(&e.name[..n]);
        }
        out
    }

    /// Sectors taken by the entry array.
    fn array_sectors(&self) -> u64 {
        (self.entries.len() * self.entry_size).div_ceil(self.sector as usize) as u64
    }

    /// Last sector usable by partitions on a disk of `disk_size` bytes.
    pub fn last_usable(&self, disk_size: u64) -> u64 {
        disk_size / self.sector - 2 - self.array_sectors()
    }

    fn header(&self, my_lba: u64, alt_lba: u64, array_lba: u64, last_usable: u64, array_crc: u32) -> Vec<u8> {
        let mut h = vec![0u8; self.sector as usize];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        h[40..48].copy_from_slice(&(2 + self.array_sectors()).to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&self.disk_guid);
        h[72..80].copy_from_slice(&array_lba.to_le_bytes());
        h[80..84].copy_from_slice(&(self.entries.len() as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(self.entry_size as u32).to_le_bytes());
        h[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    }

    /// Writes protective MBR, primary and backup GPT for a disk of `disk_size` bytes. The
    /// protective MBR is written only if `mbr` is true (otherwise sector 0 is left as is, e.g.
    /// with the boot code that came with the image).
    pub fn write(&self, dev: &File, disk_size: u64, mbr: bool) -> io::Result<()> {
        let sectors = disk_size / self.sector;
        let last = sectors - 1;
        let array = self.array();
        let crc = crc32(&array);
        let last_usable = self.last_usable(disk_size);
        if let Some(e) = self.entries.iter().find(|e| e.is_used() && e.last_lba > last_usable) {
            return Err(io::Error::other(format!("partition {}..{} beyond the end of the disk", e.first_lba, e.last_lba)));
        }
        let backup_array_lba = last - self.array_sectors();
        if mbr {
            let mut m = vec![0u8; self.sector as usize];
            let p = &mut m[446..462];
            p[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
            p[4] = 0xEE;
            p[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
            p[8..12].copy_from_slice(&1u32.to_le_bytes());
            p[12..16].copy_from_slice(&(last.min(u32::MAX as u64) as u32).to_le_bytes());
            m[510] = 0x55;
            m[511] = 0xAA;
            dev.write_all_at(&m, 0)?;
        }
        dev.write_all_at(&array, 2 * self.sector)?;
        dev.write_all_at(&self.header(1, last, 2, last_usable, crc), self.sector)?;
        dev.write_all_at(&array, backup_array_lba * self.sector)?;
        dev.write_all_at(&self.header(last, 1, backup_array_lba, last_usable, crc), last * self.sector)?;
        Ok(())
    }

    /// An empty table with 128 slots.
    pub fn new(sector: u64) -> Gpt {
        let empty = Entry { type_guid: [0; 16], guid: [0; 16], first_lba: 0, last_lba: 0, attributes: 0, name: vec![0; 72] };
        Gpt { sector, disk_guid: random_guid(), entries: vec![empty; 128], entry_size: 128 }
    }

    /// Free areas as (first, last) sectors, at least 1 MiB aligned, on a disk of `disk_size` bytes.
    pub fn free(&self, disk_size: u64) -> Vec<(u64, u64)> {
        let align = (1 << 20) / self.sector;
        let first_usable = 2 + self.array_sectors();
        let last_usable = self.last_usable(disk_size);
        let mut used: Vec<(u64, u64)> = self.used().map(|(_, e)| (e.first_lba, e.last_lba)).collect();
        used.sort();
        let mut out = Vec::new();
        let mut pos = first_usable;
        for (a, b) in used.into_iter().chain([(last_usable + 1, last_usable + 1)]) {
            let start = pos.div_ceil(align) * align;
            if a > start && a - start >= align {
                out.push((start, a - 1));
            }
            pos = pos.max(b + 1);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn roundtrip() {
        let path = std::env::temp_dir().join(format!("abb-restore-gpt-{}", std::process::id()));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        let size = 64 << 20;
        f.set_len(size).unwrap();
        let mut g = Gpt::new(512);
        g.entries[0] = Entry { type_guid: ESP, guid: random_guid(), first_lba: 2048, last_lba: 4095, attributes: 0, name: vec![0; 72] };
        g.write(&f, size, true).unwrap();
        let back = Gpt::read(&f, 512).unwrap().unwrap();
        assert_eq!(back.entries[0], g.entries[0]);
        assert_eq!(back.free(size), vec![(4096, g.last_usable(size))]);
        // Backup header at the last sector, pointing back to LBA 1.
        let mut last = vec![0u8; 512];
        f.read_exact_at(&mut last, size - 512).unwrap();
        assert_eq!(&last[0..8], b"EFI PART");
        assert_eq!(u64_at(&last, 32), 1);
        std::fs::remove_file(path).unwrap();
    }
}
