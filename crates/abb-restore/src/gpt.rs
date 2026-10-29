//! GUID partition tables: reading, and writing them back for a disk of a different size or
//! with new partitions.

use std::fs::File;
use std::io::{self, Seek, SeekFrom};
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

/// Most slots accepted in a table (the usual is 128).
const MAX_ENTRIES: usize = 4096;
/// Smallest entry array the specification allows.
const MIN_ARRAY: usize = 16 * 1024;

/// The fields of a valid table header.
struct Header {
    my_lba: u64,
    disk_guid: [u8; 16],
    array_lba: u64,
    count: usize,
    size: usize,
    array_crc: u32,
}

/// Checks the header in `h` (one sector).
fn parse_header(h: &[u8], sector: u64) -> Option<Header> {
    let s = sector as usize;
    let h = h.get(..s)?;
    if s < 92 || &h[0..8] != b"EFI PART" {
        return None;
    }
    let hsize = u32_at(h, 12) as usize;
    if !(92..=s).contains(&hsize) {
        return None;
    }
    let mut hdr = h[..hsize].to_vec();
    let crc = u32_at(&hdr, 16);
    hdr[16..20].fill(0);
    if crc32(&hdr) != crc {
        return None;
    }
    let (count, size) = (u32_at(h, 80) as usize, u32_at(h, 84) as usize);
    if !(1..=MAX_ENTRIES).contains(&count) || !(128..=4096).contains(&size) || size % 8 != 0 {
        return None;
    }
    Some(Header {
        my_lba: u64_at(h, 24),
        disk_guid: h[56..72].try_into().unwrap(),
        array_lba: u64_at(h, 72),
        count,
        size,
        array_crc: u32_at(h, 88),
    })
}

impl Gpt {
    /// Reads the table whose header is at `lba`; `read(offset, length)` gives bytes of the disk.
    fn load(read: &dyn Fn(u64, usize) -> Option<Vec<u8>>, sector: u64, lba: u64) -> Option<Gpt> {
        let h = parse_header(&read(lba.checked_mul(sector)?, sector as usize)?, sector)?;
        let array_sectors = (h.count * h.size).div_ceil(sector as usize) as u64;
        // The header must be where it says, and the array must not cover it.
        if h.my_lba != lba || (h.array_lba <= lba && lba < h.array_lba.checked_add(array_sectors)?) {
            return None;
        }
        let array = read(h.array_lba.checked_mul(sector)?, h.count * h.size)?;
        if crc32(&array) != h.array_crc {
            return None;
        }
        let entries = array
            .chunks(h.size)
            .map(|e| Entry {
                type_guid: e[0..16].try_into().unwrap(),
                guid: e[16..32].try_into().unwrap(),
                first_lba: u64_at(e, 32),
                last_lba: u64_at(e, 40),
                attributes: u64_at(e, 48),
                name: e[56..128].to_vec(),
            })
            .collect();
        Some(Gpt { sector, disk_guid: h.disk_guid, entries, entry_size: h.size })
    }

    /// Parses the primary GPT from the start of a disk (`buf` from byte 0, at least up to the end
    /// of the entry array). Returns None if there is no valid GPT.
    pub fn parse(buf: &[u8], sector: u64) -> Option<Gpt> {
        let read = |o: u64, n: usize| buf.get(usize::try_from(o).ok()?..usize::try_from(o).ok()?.checked_add(n)?).map(<[u8]>::to_vec);
        Gpt::load(&read, sector, 1)
    }

    /// Reads the GPT of a device: the primary one, or the backup at the end of the disk if the
    /// primary is damaged.
    pub fn read(dev: &File, sector: u64) -> io::Result<Option<Gpt>> {
        let read = |o: u64, n: usize| {
            let mut buf = vec![0u8; n];
            dev.read_exact_at(&mut buf, o).ok().map(|_| buf)
        };
        if let Some(g) = Gpt::load(&read, sector, 1) {
            return Ok(Some(g));
        }
        let size = (&*dev).seek(SeekFrom::End(0))?;
        let sectors = size / sector;
        Ok(if sectors > 2 { Gpt::load(&read, sector, sectors - 1) } else { None })
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

    /// Last sector usable by partitions on a disk of `disk_size` bytes (0 if the tables do not
    /// even fit).
    pub fn last_usable(&self, disk_size: u64) -> u64 {
        (disk_size / self.sector).saturating_sub(2 + self.array_sectors())
    }

    /// Bytes a disk needs after its partitions for the backup table, with the smallest entry
    /// array that [`Gpt::fit`] can leave.
    pub fn tail(sector: u64) -> u64 {
        (1 + (MIN_ARRAY as u64).div_ceil(sector)) * sector
    }

    /// Drops unused slots at the end of the entry array (keeping the 16 KiB the specification
    /// asks for) if the backup table would otherwise not fit after the partitions on a disk of
    /// `disk_size` bytes.
    pub fn fit(&mut self, disk_size: u64) {
        let keep = MIN_ARRAY.div_ceil(self.entry_size).max(self.entries.iter().rposition(Entry::is_used).map_or(0, |i| i + 1));
        let end = self.used().map(|(_, e)| e.last_lba).max().unwrap_or(0);
        while self.entries.len() > keep && end > self.last_usable(disk_size) {
            self.entries.pop();
        }
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
    /// The data is on the disk when it returns (synced).
    pub fn write(&self, dev: &File, disk_size: u64, mbr: bool) -> io::Result<()> {
        let sectors = disk_size / self.sector;
        if sectors < 3 + 2 * self.array_sectors() {
            return Err(io::Error::other("disk too small for a partition table"));
        }
        let last = sectors - 1;
        let array = self.array();
        let crc = crc32(&array);
        let last_usable = self.last_usable(disk_size);
        if let Some(e) = self.entries.iter().find(|e| e.is_used() && (e.last_lba > last_usable || e.first_lba < 2 + self.array_sectors())) {
            return Err(io::Error::other(format!("partition {}..{} outside the usable area of the disk", e.first_lba, e.last_lba)));
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
        dev.sync_all()
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
            pos = pos.max(b.saturating_add(1));
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

    fn temp(name: &str, size: u64) -> (std::path::PathBuf, File) {
        let path = std::env::temp_dir().join(format!("abb-restore-gpt-{name}-{}", std::process::id()));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        f.set_len(size).unwrap();
        (path, f)
    }

    fn table(sector: u64) -> Gpt {
        let mut g = Gpt::new(sector);
        g.entries[0] = Entry { type_guid: LINUX_FS, guid: random_guid(), first_lba: 1 << 20 >> sector.trailing_zeros(), last_lba: (8 << 20 >> sector.trailing_zeros()) - 1, attributes: 0, name: vec![0; 72] };
        g
    }

    #[test]
    fn damaged_primary_uses_the_backup() {
        let size = 64 << 20;
        let (path, f) = temp("backup", size);
        let g = table(512);
        g.write(&f, size, true).unwrap();
        // Header CRC of the primary broken.
        f.write_all_at(&[0xff; 4], 512 + 16).unwrap();
        assert!(Gpt::parse(&{ let mut b = vec![0u8; 34 * 512]; f.read_exact_at(&mut b, 0).unwrap(); b }, 512).is_none());
        let back = Gpt::read(&f, 512).unwrap().unwrap();
        assert_eq!(back.entries[0], g.entries[0]);
        // Both broken: no table.
        f.write_all_at(&[0xff; 4], size - 512 + 16).unwrap();
        assert!(Gpt::read(&f, 512).unwrap().is_none());
        std::fs::remove_file(path).unwrap();
    }

    /// The first 34 sectors of a disk with `g`, its primary header changed by `edit` (the header
    /// CRC made right again).
    fn edited(g: &Gpt, edit: impl Fn(&mut [u8])) -> Vec<u8> {
        let mut buf = vec![0u8; 34 * 512];
        let array = g.array();
        buf[1024..1024 + array.len()].copy_from_slice(&array);
        let mut h = g.header(1, 1000, 2, 900, crc32(&array));
        edit(&mut h);
        let hsize = (u32_at(&h, 12) as usize).clamp(20, 512);
        h[16..20].fill(0);
        let crc = crc32(&h[..hsize]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        buf[512..1024].copy_from_slice(&h);
        buf
    }

    #[test]
    fn parse_bounds() {
        let g = table(512);
        assert!(Gpt::parse(&edited(&g, |_| {}), 512).is_some());
        let set = |o: usize, v: u32| move |h: &mut [u8]| h[o..o + 4].copy_from_slice(&v.to_le_bytes());
        // Header size below 92 or over a sector.
        assert!(Gpt::parse(&edited(&g, set(12, 20)), 512).is_none());
        assert!(Gpt::parse(&edited(&g, set(12, 513)), 512).is_none());
        // Entry size below 128, not a multiple of 8, zero.
        assert!(Gpt::parse(&edited(&g, set(84, 64)), 512).is_none());
        assert!(Gpt::parse(&edited(&g, set(84, 130)), 512).is_none());
        assert!(Gpt::parse(&edited(&g, set(84, 0)), 512).is_none());
        // No slot, or far too many.
        assert!(Gpt::parse(&edited(&g, set(80, 0)), 512).is_none());
        assert!(Gpt::parse(&edited(&g, set(80, u32::MAX)), 512).is_none());
        // Header not where it says.
        assert!(Gpt::parse(&edited(&g, set(24, 5)), 512).is_none());
        // Short buffers.
        assert!(Gpt::parse(&edited(&g, |_| {})[..600], 512).is_none());
        assert!(Gpt::parse(&[], 512).is_none());
    }

    #[test]
    fn fit_drops_unused_slots() {
        let mut g = table(512);
        g.entries.resize(1024, g.entries[1].clone());
        // 1024 slots: 256 sectors of array. The partition ends 100 sectors before the end.
        let end = g.entries[0].last_lba;
        let size = (end + 101) * 512;
        assert!(g.last_usable(size) < end);
        g.fit(size);
        // Only as many slots dropped as needed.
        assert_eq!(g.last_usable(size), end);
        let (path, f) = temp("fit", size);
        g.write(&f, size, true).unwrap();
        assert_eq!(Gpt::read(&f, 512).unwrap().unwrap().entries.len(), g.entries.len());
        std::fs::remove_file(path).unwrap();
        // Never below 16 KiB of array: a partition too close to the end stays an error.
        let mut g = table(512);
        g.fit((end + 10) * 512);
        assert_eq!(g.entries.len(), 128);
    }

    #[test]
    fn small_disk() {
        let (path, f) = temp("small", 8192);
        assert!(Gpt::new(512).write(&f, 8192, true).is_err());
        assert_eq!(Gpt::new(512).last_usable(8192), 0);
        std::fs::remove_file(path).unwrap();
    }
}
