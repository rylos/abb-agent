//! Volumes imaged consistently: btrfs (agent snapshot, verified against the checksums) and
//! ext2/3/4 (unmounted, mounted read-only, or frozen for the whole upload).

use crate::disk::{Fs, Partition};
use abb_proto::transport::{Error, Result};
use btrfs_image::cbt::{self, CbtMeta, Digests};
use btrfs_image::image::fill_window;
use btrfs_image::prepare::{Prepared, prepare};
use ext_image::ExtFs;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};

const FIFREEZE: libc::c_ulong = 0xC004_5877;
const FITHAW: libc::c_ulong = 0xC004_5878;

/// Paths that cannot be on a frozen filesystem: the agent writes there (state, digests,
/// logs via journald) and would hang.
const NEVER_FROZEN: &[&str] = &["/", "/var", "/var/log", "/var/lib/abb-agent", "/tmp", "/run"];

/// Frozen filesystem (FIFREEZE): thawed when dropped.
struct Freeze {
    dir: std::fs::File,
}

impl Freeze {
    fn new(mount: &str) -> Result<Freeze> {
        let dev = std::fs::metadata(mount)?.dev();
        for p in NEVER_FROZEN {
            if std::fs::metadata(p).is_ok_and(|m| m.dev() == dev) {
                return Err(Error::Protocol(format!("{mount}: contains {p}, cannot be frozen")));
            }
        }
        let dir = std::fs::File::open(mount)?;
        let mut arg: libc::c_int = 0;
        if unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&dir), FIFREEZE, &mut arg) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Freeze { dir })
    }
}

impl Drop for Freeze {
    fn drop(&mut self) {
        let mut arg: libc::c_int = 0;
        if unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&self.dir), FITHAW, &mut arg) } != 0 {
            eprintln!("thaw: {}", std::io::Error::last_os_error());
        }
    }
}

/// An ext can be read consistently if it is unmounted, mounted read-only, or (with
/// `freeze`) can be frozen.
pub fn ext_readable(p: &Partition, freeze: bool) -> bool {
    p.mount_path.is_empty() || p.read_only || freeze
}

pub struct ExtVol {
    pub fs: ExtFs,
    /// Digests of the 64 KiB blocks, computed from the data while it is sent.
    table: Vec<AtomicU64>,
    _freeze: Option<Freeze>,
}

#[allow(clippy::large_enum_variant)] // few volumes, never copied
pub enum Vol {
    Btrfs { prep: Prepared, digests: Digests },
    Ext(ExtVol),
}

fn ext_digest(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data).max(1)
}

impl Vol {
    pub fn prepare(p: &Partition, run_name: &str) -> Result<Vol> {
        let name = crate::backup::alias(p);
        match p.fs {
            Fs::Btrfs => {
                let exclude = &crate::config::get().btrfs.exclude_subvolumes;
                let uuid = p.fs_uuid.clone();
                let mut record = |paths: &[std::path::PathBuf]| {
                    if let Err(e) = crate::fixup::record(&uuid, paths) {
                        eprintln!("recording excluded subvolumes: {e}");
                    }
                };
                let prep = prepare(&p.device(), &p.fs_uuid, run_name, exclude, &mut record)
                    .map_err(|e| Error::Protocol(format!("preparing {name}: {e}")))?;
                let digests = Digests::compute(&prep.fs, &prep.cap, &prep.map)
                    .map_err(|e| Error::Protocol(format!("digests of {name}: {e}")))?;
                eprintln!(
                    "{name}: generation {}, capture {} nodes + {} extents, {:.1} GiB used",
                    prep.fs.sb.generation,
                    prep.cap.nodes.len(),
                    prep.cap.data.len(),
                    prep.map.stats.physical_bytes as f64 / (1u64 << 30) as f64
                );
                for x in &prep.excluded {
                    eprintln!("{name}: subvolume {} left out", x.display());
                }
                for u in &prep.unpinned {
                    eprintln!("{name}: {} not snapshotted (active swapfile?): inconsistent data, partial backup", u.display());
                }
                Ok(Vol::Btrfs { prep, digests })
            }
            Fs::Ext(_) => {
                // Freeze first, then read the map: it must hold for the whole upload.
                let freeze = if p.mount_path.is_empty() || p.read_only { None } else { Some(Freeze::new(&p.mount_path)?) };
                let fs = ExtFs::open(&p.device()).map_err(|e| Error::Protocol(format!("{name}: {e}")))?;
                eprintln!(
                    "{name}: ext, {:.1} GiB used{}",
                    fs.used_bytes() as f64 / (1u64 << 30) as f64,
                    if freeze.is_some() { ", frozen until the upload ends" } else { "" }
                );
                let n = fs.size.div_ceil(cbt::BLOCK) as usize;
                Ok(Vol::Ext(ExtVol { fs, table: (0..n).map(|_| AtomicU64::new(0)).collect(), _freeze: freeze }))
            }
            _ => Err(Error::Protocol(format!("{name}: unsupported filesystem"))),
        }
    }

    /// Volume size in bytes (offsets beyond it are outside the filesystem).
    pub fn size(&self) -> u64 {
        match self {
            Vol::Btrfs { prep, .. } => prep.fs.sb.total_bytes,
            Vol::Ext(e) => e.fs.size,
        }
    }

    /// `[s, e)` (volume offsets) contains used bytes.
    pub fn any_used(&self, s: u64, e: u64) -> bool {
        match self {
            Vol::Btrfs { prep, .. } => {
                let r = &prep.map.ranges;
                let k = r.partition_point(|x| x.physical + x.len <= s.saturating_sub(128 << 20));
                r[k..].iter().take_while(|x| x.physical < e).any(|x| x.physical + x.len > s)
            }
            Vol::Ext(x) => x.fs.any_used(s, e),
        }
    }

    /// For an incremental: `Some(changed)` if already known from the digests, `None` if it
    /// must be decided by reading the block (ext).
    pub fn changed(&self, prev: &Digests, k: usize) -> Option<bool> {
        match self {
            Vol::Btrfs { digests, .. } => Some(digests.changed(prev, k)),
            Vol::Ext(_) => None,
        }
    }

    pub fn block_used(&self, k: usize) -> bool {
        match self {
            Vol::Btrfs { digests, .. } => digests.used(k),
            Vol::Ext(x) => x.fs.any_used(k as u64 * cbt::BLOCK, (k as u64 + 1) * cbt::BLOCK),
        }
    }

    /// Reassembles `seg` (bytes read from the device starting at volume offset `vs`): unused
    /// areas zeroed and, for btrfs, capture, superblock and verification of every whole 64 KiB
    /// block against the checksums; for ext, the digest of every whole block is recorded.
    /// `false` if there are no used bytes.
    pub fn fill(&self, vs: u64, seg: &mut [u8]) -> std::result::Result<bool, String> {
        let any = match self {
            Vol::Btrfs { prep, .. } => fill_window(&prep.fs, &prep.cap, &prep.map, vs, seg).map_err(|e| e.to_string())?,
            Vol::Ext(x) => x.fs.fill_window(vs, seg),
        };
        if !any {
            seg.fill(0);
            return Ok(false);
        }
        let ve = vs + seg.len() as u64;
        let mut k = vs.div_ceil(cbt::BLOCK);
        loop {
            let (bs, be) = (k * cbt::BLOCK, ((k + 1) * cbt::BLOCK).min(self.size()));
            if bs >= be || be > ve {
                break;
            }
            let block = &seg[(bs - vs) as usize..(be - vs) as usize];
            match self {
                Vol::Btrfs { prep, digests } => digests.verify_block(&prep.map, k as usize, block).map_err(|e| e.to_string())?,
                Vol::Ext(x) => {
                    let v = if self.block_used(k as usize) { ext_digest(block) } else { 0 };
                    x.table[k as usize].store(v, Ordering::Relaxed);
                }
            }
            k += 1;
        }
        Ok(true)
    }

    /// For ext after `fill`: the digest of block `k` equals the previous backup's.
    pub fn unchanged(&self, prev: &Digests, k: usize) -> bool {
        match self {
            Vol::Btrfs { .. } => false,
            Vol::Ext(x) => prev.table.get(k).is_some_and(|&p| p != 0 && p == x.table[k].load(Ordering::Relaxed)),
        }
    }

    /// Digests to save as the base for the next incremental.
    pub fn save_digests(&self, path: &std::path::Path, version_id: u64, partition_offset: u64) -> std::io::Result<()> {
        match self {
            Vol::Btrfs { prep, digests } => {
                digests.save(path, &CbtMeta::new(version_id, prep.fs.sb.generation, partition_offset, digests))
            }
            Vol::Ext(x) => {
                let d = Digests::from_table(x.table.iter().map(|a| a.load(Ordering::Relaxed)).collect());
                d.save(path, &CbtMeta::new(version_id, x.fs.wtime as u64, partition_offset, &d))
            }
        }
    }

    /// Btrfs subvolumes that could not be snapshotted (partial backup).
    pub fn incomplete(&self) -> bool {
        matches!(self, Vol::Btrfs { prep, .. } if !prep.unpinned.is_empty())
    }

    pub fn cleanup(self) {
        if let Vol::Btrfs { prep, .. } = self
            && let Err(e) = prep.cleanup()
        {
            eprintln!("snapshot cleanup: {e}");
        }
    }
}
