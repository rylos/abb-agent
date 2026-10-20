//! Disks of the machine being restored.

use crate::gpt::Gpt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct LocalPartition {
    pub number: u64,
    pub name: String,
    pub offset: u64,
    pub length: u64,
    /// Mounted, swap or held by another device.
    pub busy: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LocalDisk {
    /// Kernel name, e.g. "nvme0n1".
    pub name: String,
    pub size: u64,
    pub sector_size: u64,
    pub model: String,
    pub removable: bool,
    /// Why it cannot be written to (mounted, swap, part of a RAID or LVM…), if so.
    pub busy: Option<String>,
    pub partitions: Vec<LocalPartition>,
    pub gpt: Option<Gpt>,
}

impl LocalDisk {
    pub fn path(&self) -> PathBuf {
        Path::new("/dev").join(&self.name)
    }

    /// Device of partition `n`, e.g. /dev/nvme0n1p2 or /dev/sda2.
    pub fn partition_path(&self, n: u64) -> PathBuf {
        let sep = if self.name.ends_with(|c: char| c.is_ascii_digit()) { "p" } else { "" };
        Path::new("/dev").join(format!("{}{sep}{n}", self.name))
    }
}

fn read(p: impl AsRef<Path>) -> String {
    fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn read_u64(p: impl AsRef<Path>) -> u64 {
    read(p).parse().unwrap_or(0)
}

/// Why a block device is in use: mounted, swap, or held by another device (dm, md).
fn busy_reason(name: &str) -> Option<String> {
    let dev = format!("/dev/{name}");
    let mounts = read("/proc/mounts");
    if let Some(l) = mounts.lines().find(|l| l.split(' ').next() == Some(dev.as_str())) {
        return Some(format!("{dev} is mounted on {}", l.split(' ').nth(1).unwrap_or("?")));
    }
    if read("/proc/swaps").lines().any(|l| l.split_whitespace().next() == Some(dev.as_str())) {
        return Some(format!("{dev} is in use as swap"));
    }
    let holders = Path::new("/sys/class/block").join(name).join("holders");
    if let Some(h) = fs::read_dir(holders).ok().and_then(|mut d| d.next()).and_then(Result::ok) {
        return Some(format!("{dev} is used by {}", h.file_name().to_string_lossy()));
    }
    None
}

pub fn disks() -> Vec<LocalDisk> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/sys/block") else { return out };
    let mut names: Vec<String> = dir.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    for name in names {
        if ["loop", "ram", "zram", "sr", "fd", "dm-", "md", "nbd"].iter().any(|p| name.starts_with(p))
            || name.contains("boot")
            || name.contains("rpmb")
        {
            continue;
        }
        let sys = Path::new("/sys/block").join(&name);
        let size = read_u64(sys.join("size")) * 512;
        if size == 0 {
            continue;
        }
        let mut partitions: Vec<LocalPartition> = fs::read_dir(&sys)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| e.path().join("partition").exists())
            .map(|e| {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                LocalPartition {
                    number: read_u64(p.join("partition")),
                    busy: busy_reason(&name),
                    name,
                    offset: read_u64(p.join("start")) * 512,
                    length: read_u64(p.join("size")) * 512,
                }
            })
            .collect();
        partitions.sort_by_key(|p| p.offset);
        let sector_size = read_u64(sys.join("queue/logical_block_size")).max(512);
        let gpt = fs::File::open(Path::new("/dev").join(&name)).ok().and_then(|f| Gpt::read(&f, sector_size).ok().flatten());
        let model = [read(sys.join("device/vendor")), read(sys.join("device/model"))]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        out.push(LocalDisk {
            busy: busy_reason(&name).or_else(|| partitions.iter().find_map(|p| p.busy.clone())),
            removable: read(sys.join("removable")) == "1",
            name,
            size,
            sector_size,
            model,
            partitions,
            gpt,
        });
    }
    out
}

/// SMBIOS UUID of this machine (the server records which machine was restored).
pub fn machine_uuid() -> String {
    let u = read("/sys/class/dmi/id/product_uuid").to_uppercase();
    if u.is_empty() { "00000000-0000-0000-0000-000000000000".into() } else { u }
}

pub fn size_str(b: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", units[i]) }
}
