//! Discovery of disks, GPT partitions, filesystems and mounts, without external programs
//! (sysfs, GPT table read from the disk, filesystem superblocks, /proc/self/mountinfo).

use btrfs_image::device::Device;
use std::fs;
use std::path::{Path, PathBuf};

pub const GUID_ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
pub const GUID_BIOS_BOOT: &str = "21686148-6449-6E6F-744E-656564454649";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fs {
    None,
    Btrfs,
    Vfat,
    /// ext2, ext3 or ext4 (2, 3, 4).
    Ext(u8),
    Xfs,
    Swap,
    /// LVM physical volume.
    Lvm,
    /// LUKS encrypted container.
    Luks,
}

impl Fs {
    /// Name as reported by blkid (`file_system` field of the device spec).
    pub fn name(&self) -> &str {
        match self {
            Fs::None => "",
            Fs::Btrfs => "btrfs",
            Fs::Vfat => "vfat",
            Fs::Ext(2) => "ext2",
            Fs::Ext(3) => "ext3",
            Fs::Ext(_) => "ext4",
            Fs::Xfs => "xfs",
            Fs::Swap => "swap",
            Fs::Lvm => "LVM2_member",
            Fs::Luks => "crypto_LUKS",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Partition {
    /// e.g. "nvme0n1p2"
    pub name: String,
    pub number: u32,
    /// Bytes from the start of the disk.
    pub offset: u64,
    pub length: u64,
    pub type_guid: String,
    pub fs: Fs,
    /// Filesystem UUID (btrfs/ext) or FAT serial "XXXX-XXXX".
    pub fs_uuid: String,
    /// Main mount point ("/" if present), empty if not mounted.
    pub mount_path: String,
    /// Mounted read-only (meaningful only if mounted).
    pub read_only: bool,
}

impl Partition {
    pub fn device(&self) -> PathBuf {
        PathBuf::from("/dev").join(&self.name)
    }

    pub fn is_esp(&self) -> bool {
        self.type_guid == GUID_ESP
    }

    pub fn is_bios_boot(&self) -> bool {
        self.type_guid == GUID_BIOS_BOOT
    }

    /// `attribute` mask of the device spec (1 normal, |8 ESP, |0x40 BIOS boot).
    pub fn attribute(&self) -> u32 {
        if self.is_esp() {
            9
        } else if self.is_bios_boot() {
            65
        } else {
            1
        }
    }
}

#[derive(Clone, Debug)]
pub struct Disk {
    /// e.g. "nvme0n1"
    pub name: String,
    pub size: u64,
    pub sector_size: u32,
    /// "GPT" (MBR not supported yet).
    pub layout: String,
    /// Sorted by offset.
    pub partitions: Vec<Partition>,
}

impl Disk {
    pub fn device(&self) -> PathBuf {
        PathBuf::from("/dev").join(&self.name)
    }

    /// Image name on the server ("nvme0n1.img").
    pub fn image_name(&self) -> String {
        format!("{}.img", self.name)
    }
}

fn read_trim(p: impl AsRef<Path>) -> std::io::Result<String> {
    Ok(fs::read_to_string(p)?.trim().to_string())
}

fn read_u64(p: impl AsRef<Path>) -> std::io::Result<u64> {
    read_trim(&p)?
        .parse()
        .map_err(|_| std::io::Error::other(format!("{}: invalid number", p.as_ref().display())))
}

/// Disk containing partition `part` (e.g. "nvme0n1p2" → "nvme0n1").
pub fn parent_disk(part: &str) -> std::io::Result<String> {
    let link = fs::canonicalize(Path::new("/sys/class/block").join(part))?;
    let parent = link.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
    match parent {
        Some(p) if Path::new("/sys/block").join(&p).exists() => Ok(p),
        _ => Ok(part.to_string()),
    }
}

/// Disk containing the filesystem mounted at `/`.
pub fn system_disk() -> std::io::Result<String> {
    let mounts = mounts()?;
    let root = mounts
        .iter()
        .find(|m| m.target == "/")
        .ok_or_else(|| std::io::Error::other("mount of / not found"))?;
    let dev = fs::canonicalize(&root.source)?;
    parent_disk(&dev.file_name().unwrap().to_string_lossy())
}

pub struct Mount {
    pub source: String,
    pub target: String,
    /// Superblock options ("rw,…" or "ro,…").
    pub super_options: String,
}

pub fn mounts() -> std::io::Result<Vec<Mount>> {
    let text = fs::read_to_string("/proc/self/mountinfo")?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let (a, b) = l.split_once(" - ")?;
            let target = unescape(a.split(' ').nth(4)?);
            let source = unescape(b.split(' ').nth(1)?);
            let super_options = b.split(' ').nth(2).unwrap_or("").to_string();
            Some(Mount { source, target, super_options })
        })
        .collect())
}

/// mountinfo encodes spaces and the like as \040.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len()
            && let Ok(n) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(n as char);
                i += 4;
                continue;
            }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

pub fn scan(name: &str) -> std::io::Result<Disk> {
    let sys = Path::new("/sys/block").join(name);
    let sector_size = read_u64(sys.join("queue/logical_block_size"))? as u32;
    let size = read_u64(sys.join("size"))? * 512;
    let dev = Device::open(&Path::new("/dev").join(name))?;
    let gpt = read_gpt(&dev, sector_size as u64)?;
    let mounts = mounts()?;

    let mut partitions = Vec::new();
    for entry in fs::read_dir(&sys)? {
        let entry = entry?;
        let pname = entry.file_name().to_string_lossy().into_owned();
        let psys = entry.path();
        if !psys.join("partition").exists() {
            continue;
        }
        let number = read_u64(psys.join("partition"))? as u32;
        let offset = read_u64(psys.join("start"))? * 512;
        let length = read_u64(psys.join("size"))? * 512;
        let type_guid = gpt.iter().find(|(n, _)| *n == number).map(|(_, g)| g.clone()).unwrap_or_default();
        let (fs, fs_uuid) = probe_fs(&dev, offset, length)?;
        let pdev = format!("/dev/{pname}");
        let mine: Vec<&Mount> =
            mounts.iter().filter(|m| fs::canonicalize(&m.source).is_ok_and(|p| p == Path::new(&pdev))).collect();
        let read_only = mine.iter().all(|m| m.super_options.split(',').any(|o| o == "ro"));
        let mut targets: Vec<&str> = mine.iter().map(|m| m.target.as_str()).collect();
        targets.sort_by_key(|t| (*t != "/", t.len()));
        partitions.push(Partition {
            name: pname,
            number,
            offset,
            length,
            type_guid,
            fs,
            fs_uuid,
            mount_path: targets.first().map(|t| t.to_string()).unwrap_or_default(),
            read_only,
        });
    }
    partitions.sort_by_key(|p| p.offset);
    Ok(Disk { name: name.to_string(), size, sector_size, layout: "GPT".into(), partitions })
}

fn guid(b: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{}",
        u32::from_le_bytes(b[0..4].try_into().unwrap()),
        u16::from_le_bytes(b[4..6].try_into().unwrap()),
        u16::from_le_bytes(b[6..8].try_into().unwrap()),
        b[8],
        b[9],
        b[10..16].iter().map(|x| format!("{x:02X}")).collect::<String>()
    )
}

/// (partition number, type GUID) from the primary GPT table.
fn read_gpt(dev: &Device, sector: u64) -> std::io::Result<Vec<(u32, String)>> {
    let mut hdr = vec![0u8; sector as usize];
    dev.read_exact_at(&mut hdr, sector)?;
    if &hdr[0..8] != b"EFI PART" {
        return Err(std::io::Error::other("the disk has no GPT table (MBR not supported yet)"));
    }
    let entries_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
    let count = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
    let esize = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
    let mut table = vec![0u8; count * esize];
    dev.read_exact_at(&mut table, entries_lba * sector)?;
    Ok((0..count)
        .filter_map(|i| {
            let e = &table[i * esize..(i + 1) * esize];
            (e[..16] != [0u8; 16]).then(|| (i as u32 + 1, guid(&e[..16])))
        })
        .collect())
}

fn uuid_str(b: &[u8]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Filesystem type and UUID from known superblocks.
fn probe_fs(dev: &Device, offset: u64, length: u64) -> std::io::Result<(Fs, String)> {
    if length < 128 * 1024 {
        return Ok((Fs::None, String::new()));
    }
    let mut head = vec![0u8; 128 * 1024];
    dev.read_exact_at(&mut head, offset)?;
    if &head[0x10040..0x10048] == b"_BHRfS_M" {
        return Ok((Fs::Btrfs, uuid_str(&head[0x10020..0x10030])));
    }
    if u16::from_le_bytes(head[1080..1082].try_into().unwrap()) == 0xEF53 {
        let sb = &head[1024..2048];
        let feat = |o: usize| u32::from_le_bytes(sb[o..o + 4].try_into().unwrap());
        // ext4: extent, 64bit, flex_bg, huge_file, … ; ext3: journal.
        let version = if feat(0x60) & !0x16 != 0 || feat(0x64) & !0x7 != 0 {
            4
        } else if feat(0x5C) & 0x4 != 0 {
            3
        } else {
            2
        };
        return Ok((Fs::Ext(version), uuid_str(&sb[0x68..0x78])));
    }
    if &head[0..4] == b"XFSB" {
        return Ok((Fs::Xfs, uuid_str(&head[32..48])));
    }
    if head[510] == 0x55 && head[511] == 0xAA {
        let serial = |o: usize| {
            let s = u32::from_le_bytes(head[o..o + 4].try_into().unwrap());
            format!("{:04X}-{:04X}", s >> 16, s & 0xffff)
        };
        if &head[0x52..0x57] == b"FAT32" {
            return Ok((Fs::Vfat, serial(0x43)));
        }
        if &head[0x36..0x39] == b"FAT" {
            return Ok((Fs::Vfat, serial(0x27)));
        }
    }
    if &head[512..520] == b"LABELONE" && &head[536..544] == b"LVM2 001" {
        return Ok((Fs::Lvm, String::new()));
    }
    if &head[0..6] == b"LUKS\xba\xbe" {
        return Ok((Fs::Luks, String::new()));
    }
    if &head[4086..4096] == b"SWAPSPACE2" {
        return Ok((Fs::Swap, uuid_str(&head[1036..1052])));
    }
    Ok((Fs::None, String::new()))
}
