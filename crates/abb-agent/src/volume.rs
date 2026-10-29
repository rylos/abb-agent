//! Volumes imaged consistently: btrfs (agent snapshot, verified against the checksums) and
//! ext2/3/4 (unmounted, read-only, or mounted read-write: frozen just long enough to capture
//! the metadata, or for the whole upload if configured so).

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

/// Paths that cannot be on a filesystem frozen for long: the agent writes there (state,
/// digests, logs via journald) and would hang.
const NEVER_FROZEN: &[&str] = &["/", "/var", "/var/log", "/var/lib/abb-agent", "/tmp", "/run"];

/// Filesystems the agent froze, one file per device number holding the mount path, so that a
/// start after the agent was killed while one was frozen can thaw it (`thaw_leftovers`). In /run:
/// never frozen, and gone after a reboot like the freeze itself.
const FROZEN_DIR: &str = "/run/abb-agent/frozen";

/// Frozen filesystem (FIFREEZE): thawed when dropped.
struct Freeze {
    dir: std::fs::File,
    mount: String,
    marker: std::path::PathBuf,
}

impl Freeze {
    /// Whether the filesystem at `mount` holds a path the agent or the system writes to all the
    /// time: it can only be frozen for an instant.
    fn holds_live_paths(mount: &str) -> bool {
        let Ok(dev) = std::fs::metadata(mount).map(|m| m.dev()) else { return true };
        NEVER_FROZEN.iter().any(|p| std::fs::metadata(p).is_ok_and(|m| m.dev() == dev))
    }

    fn new(mount: &str) -> Result<Freeze> {
        let dir = std::fs::File::open(mount)?;
        let marker = std::path::Path::new(FROZEN_DIR).join(dir.metadata()?.dev().to_string());
        std::fs::create_dir_all(FROZEN_DIR)?;
        std::fs::write(&marker, mount)?;
        let mut arg: libc::c_int = 0;
        if unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&dir), FIFREEZE, &mut arg) } != 0 {
            let e = std::io::Error::last_os_error();
            let _ = std::fs::remove_file(&marker);
            return Err(e.into());
        }
        Ok(Freeze { dir, mount: mount.to_string(), marker })
    }
}

/// FITHAW on `dir`: `Ok(false)` if it was not frozen.
fn thaw(dir: &std::fs::File) -> std::io::Result<bool> {
    let mut arg: libc::c_int = 0;
    if unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(dir), FITHAW, &mut arg) } != 0 {
        let e = std::io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::EINVAL) { Ok(false) } else { Err(e) };
    }
    Ok(true)
}

impl Drop for Freeze {
    fn drop(&mut self) {
        match thaw(&self.dir) {
            Ok(_) => {
                let _ = std::fs::remove_file(&self.marker);
            }
            Err(e) => eprintln!("thaw {}: {e}", self.mount),
        }
    }
}

/// Thaws the filesystems an earlier run of the agent froze and could not thaw (killed while one
/// was frozen). Only those it recorded, and only if the same filesystem is still mounted there.
pub fn thaw_leftovers() {
    let Ok(entries) = std::fs::read_dir(FROZEN_DIR) else { return };
    for e in entries.flatten() {
        let marker = e.path();
        let dev = e.file_name().to_string_lossy().parse::<u64>().ok();
        if let (Some(dev), Ok(mount)) = (dev, std::fs::read_to_string(&marker))
            && let Ok(dir) = std::fs::File::open(&mount)
            && dir.metadata().is_ok_and(|m| m.dev() == dev)
        {
            match thaw(&dir) {
                Ok(true) => eprintln!("{mount}: thawed (left frozen by an interrupted backup)"),
                Ok(false) => {}
                Err(e) => {
                    eprintln!("thaw {mount}: {e}");
                    continue;
                }
            }
        }
        let _ = std::fs::remove_file(&marker);
    }
}

pub struct ExtVol {
    pub fs: ExtFs,
    /// Digests of the 64 KiB blocks, computed from the data while it is sent.
    table: Vec<AtomicU64>,
    /// Inodes in use and the blocks they own (for incrementals without reading unchanged files).
    inodes: Option<ext_image::Inodes>,
    /// Blocks known unchanged from the inodes (bitset over 64 KiB blocks).
    skip: Vec<u64>,
    /// Metadata read while the filesystem was frozen for an instant (mounted read-write): it
    /// replaces what is on the device, so that the image is consistent. Sorted by offset.
    overlay: Vec<(u64, Vec<u8>)>,
    /// Mount point, for a filesystem read while in use.
    mount: Option<String>,
    /// Frozen for the whole upload: thawed after the last read (`release`).
    freeze: std::sync::Mutex<Option<Freeze>>,
}

/// Snapshots and mounts of a prepared btrfs volume: released by `Vol::cleanup`, or when dropped
/// (an error before the volume is ready, a panic).
pub struct Snap(Option<Prepared>);

impl std::ops::Deref for Snap {
    type Target = Prepared;
    fn deref(&self) -> &Prepared {
        self.0.as_ref().expect("released")
    }
}

impl Snap {
    fn release(&mut self) {
        if let Some(p) = self.0.take()
            && let Err(e) = p.cleanup()
        {
            eprintln!("snapshot cleanup: {e}");
        }
    }
}

impl Drop for Snap {
    fn drop(&mut self) {
        self.release();
    }
}

#[allow(clippy::large_enum_variant)] // few volumes, never copied
pub enum Vol {
    Btrfs { prep: Snap, digests: Digests },
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
                let own: Vec<std::path::PathBuf> = crate::backup::cbt_subvolume().into_iter().collect();
                let prep = prepare(&p.device(), &p.fs_uuid, run_name, exclude, &own, &mut record)
                    .map_err(|e| Error::Protocol(format!("preparing {name}: {e}")))?;
                // From here on the snapshots go away with `prep`, whatever happens.
                let prep = Snap(Some(prep));
                let td = std::time::Instant::now();
                let digests = Digests::compute(&prep.fs, &prep.cap, &prep.map)
                    .map_err(|e| Error::Protocol(format!("digests of {name}: {e}")))?;
                eprintln!("timing: digests {:.1}s", td.elapsed().as_secs_f64());
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
                let n_err = |e: ext_image::Error| Error::Protocol(format!("{name}: {e}"));
                let mounted_rw = !(p.mount_path.is_empty() || p.read_only);
                let whole = mounted_rw && crate::config::get().ext.freeze_mounted && !Freeze::holds_live_paths(&p.mount_path);
                if mounted_rw && !whole {
                    // Frozen just long enough to read the metadata: nothing is logged or written
                    // until it is thawed.
                    let t = std::time::Instant::now();
                    let guard = Freeze::new(&p.mount_path)?;
                    let captured = (|| -> std::result::Result<_, ext_image::Error> {
                        let fs = ExtFs::open(&p.device())?;
                        let inodes = fs.inodes()?;
                        let meta = fs.metadata_ranges(&inodes);
                        let overlay = fs.read_ranges(&meta)?;
                        Ok((fs, inodes, overlay))
                    })();
                    drop(guard);
                    let frozen = t.elapsed();
                    let (fs, inodes, overlay) = captured.map_err(n_err)?;
                    let bytes: usize = overlay.iter().map(|o| o.1.len()).sum();
                    eprintln!(
                        "{name}: ext, {:.1} GiB used, frozen {:.2}s to read {:.1} MiB of metadata, files read while in use",
                        fs.used_bytes() as f64 / (1u64 << 30) as f64,
                        frozen.as_secs_f64(),
                        bytes as f64 / (1u64 << 20) as f64
                    );
                    let n = fs.size.div_ceil(cbt::BLOCK) as usize;
                    return Ok(Vol::Ext(ExtVol {
                        fs,
                        table: (0..n).map(|_| AtomicU64::new(0)).collect(),
                        inodes: Some(inodes),
                        skip: vec![],
                        overlay,
                        mount: Some(p.mount_path.clone()),
                        freeze: Default::default(),
                    }));
                }
                // Unmounted or read-only, or frozen for the whole upload: read as it is.
                let freeze = if whole { Some(Freeze::new(&p.mount_path)?) } else { None };
                let mut fs = ExtFs::open(&p.device()).map_err(n_err)?;
                // Not cleanly unmounted (or mounted with noload): the journal has transactions
                // whose blocks look free in the bitmaps. Freezing writes them out, so only the
                // filesystem read as it is can be in this state.
                if fs.needs_recovery && freeze.is_none() {
                    eprintln!("{name}: ext journal needs recovery: the whole filesystem is read, it is replayed at the first mount after a restore");
                    fs.assume_all_used();
                }
                eprintln!(
                    "{name}: ext, {:.1} GiB used{}",
                    fs.used_bytes() as f64 / (1u64 << 30) as f64,
                    if freeze.is_some() { ", frozen until the upload ends" } else { "" }
                );
                let n = fs.size.div_ceil(cbt::BLOCK) as usize;
                let inodes = fs.inodes().map_err(|e| eprintln!("{name}: reading the inodes: {e}")).ok();
                Ok(Vol::Ext(ExtVol { fs, table: (0..n).map(|_| AtomicU64::new(0)).collect(), inodes, skip: vec![], overlay: vec![], mount: None, freeze: std::sync::Mutex::new(freeze) }))
            }
            _ => Err(Error::Protocol(format!("{name}: unsupported filesystem"))),
        }
    }

    /// One line for the terminal view: space used, subvolumes left out.
    pub fn summary(&self) -> String {
        match self {
            Vol::Btrfs { prep, .. } => {
                // Copies in the service snapshots and the agent's own CBT subvolume are not the
                // user's exclusions.
                let n = prep
                    .excluded
                    .iter()
                    .filter(|x| !x.starts_with(".abb-snapshots") && !x.ends_with("abb-agent/cbt"))
                    .count();
                let mut s = format!("{} used", crate::ui::size(prep.map.stats.physical_bytes));
                if n > 0 {
                    s += &format!(" · {n} subvolume{} left out", if n == 1 { "" } else { "s" });
                }
                if !prep.unpinned.is_empty() {
                    s += " · some files not snapshotted";
                }
                s
            }
            Vol::Ext(e) => format!("{} used", crate::ui::size(e.fs.used_bytes())),
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

    /// For the log: what changed since the previous backup (btrfs).
    pub fn change_report(&self, prev: &Digests) -> Option<String> {
        match self {
            Vol::Btrfs { prep, digests } => Some(digests.change_report(prev, &prep.fs, &prep.map)),
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
            Vol::Btrfs { prep, .. } => fill_window(&prep.fs, &prep.cap, &prep.map, vs, seg).map_err(|e| match e {
                // Which tree held it at the generation read: something the capture missed.
                btrfs_image::Error::Changed { logical, .. } => {
                    format!("{e} ({})", btrfs_image::capture::tree_block_refs(&prep.fs, logical))
                }
                e => e.to_string(),
            })?,
            Vol::Ext(x) => {
                let any = x.fs.fill_window(vs, seg);
                // Metadata captured while frozen replaces what is on the device now.
                let ve = vs + seg.len() as u64;
                let first = x.overlay.partition_point(|(s, d)| s + d.len() as u64 <= vs);
                for (s, d) in x.overlay[first..].iter().take_while(|(s, _)| *s < ve) {
                    let (a, b) = ((*s).max(vs), (s + d.len() as u64).min(ve));
                    seg[(a - vs) as usize..(b - vs) as usize].copy_from_slice(&d[(a - s) as usize..(b - s) as usize]);
                }
                any
            }
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
                Vol::Btrfs { prep, digests } => {
                    if let Err(e) = digests.verify_block(&prep.map, k as usize, block) {
                        eprintln!("{}", digests.explain_block(&prep.fs, &prep.cap, &prep.map, k as usize, block));
                        // Who references those extents now, with this run's snapshots still there.
                        for r in prep.map.ranges.iter().filter(|r| r.kind == btrfs_image::usedmap::Kind::Data && r.physical < be && r.physical + r.len > bs) {
                            let out = std::process::Command::new("btrfs")
                                .args(["inspect-internal", "logical-resolve", "-o", &r.logical.to_string()])
                                .arg(&prep.top)
                                .output();
                            if let Ok(o) = out {
                                eprintln!("now at {}: {}{}", r.logical, String::from_utf8_lossy(&o.stdout).trim(), String::from_utf8_lossy(&o.stderr).trim());
                            }
                        }
                        return Err(e.to_string());
                    }
                }
                Vol::Ext(x) => {
                    let v = if self.block_used(k as usize) { ext_digest(block) } else { 0 };
                    x.table[k as usize].store(v, Ordering::Relaxed);
                }
            }
            k += 1;
        }
        Ok(true)
    }

    /// ext: with the inodes of the previous backup, finds the 64 KiB blocks that need not be
    /// read: every used byte in them belongs to a file whose inode did not change (same ctime,
    /// mtime, size, generation, flags and block mapping, as rustic and restic decide but for
    /// the mapping), or to an inode table block whose inodes were and are all free. Active
    /// swapfiles are written without touching their inode: never skipped. Returns how many.
    pub fn plan_skip(&mut self, prev: &std::collections::HashMap<u32, [u64; 4]>) -> usize {
        let Vol::Ext(x) = self else { return 0 };
        let Some(inodes) = &x.inodes else { return 0 };
        let swap = swapfiles(x.fs.device());
        let mut safe: Vec<(u64, u64)> = inodes
            .owned
            .iter()
            .filter(|(_, _, ino)| !swap.contains(ino) && prev.get(ino).is_some_and(|p| Some(p) == inodes.sigs.get(ino)))
            .map(|&(s, l, _)| (s, l))
            .chain(
                inodes
                    .free_itable
                    .iter()
                    .filter(|&&(_, _, first, n)| (first..first + n).all(|i| !prev.contains_key(&i)))
                    .map(|&(s, l, _, _)| (s, l)),
            )
            .collect();
        safe.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(safe.len());
        for (s, l) in safe {
            match merged.last_mut() {
                Some((ms, ml)) if s <= *ms + *ml => *ml = (s + l).max(*ms + *ml) - *ms,
                _ => merged.push((s, l)),
            }
        }
        let n = x.table.len();
        x.skip = vec![0u64; n.div_ceil(64)];
        let mut count = 0;
        let mut j = 0;
        let mut k = 0usize;
        // Walk the used ranges block by block; a block is safe if all its used bytes are.
        let mut ok = vec![true; n];
        let mut any = vec![false; n];
        for &(s, l) in &x.fs.ranges {
            let mut p = s;
            while p < s + l {
                let b = (p / cbt::BLOCK) as usize;
                let e = ((b as u64 + 1) * cbt::BLOCK).min(s + l);
                any[b] = true;
                while j < merged.len() && merged[j].0 + merged[j].1 <= p {
                    j += 1;
                }
                if !(j < merged.len() && merged[j].0 <= p && merged[j].0 + merged[j].1 >= e) {
                    ok[b] = false;
                }
                p = e;
            }
        }
        while k < n {
            if any[k] && ok[k] {
                x.skip[k / 64] |= 1 << (k % 64);
                count += 1;
            }
            k += 1;
        }
        count
    }

    /// ext read while in use: how many files changed after the metadata was captured (their
    /// content in this version may be inconsistent; the next backup reads them again).
    pub fn changed_while_read(&self) -> Option<usize> {
        let Vol::Ext(x) = self else { return None };
        if x.overlay.is_empty() {
            return None;
        }
        let then = &x.inodes.as_ref()?.sigs;
        // The inode tables on the device lag behind the journal: ask the kernel instead, by
        // inode number and generation (ext's file handle).
        let dir = std::fs::File::open(x.mount.as_ref()?).ok()?;
        #[repr(C)]
        struct Handle {
            bytes: u32,
            kind: i32,
            ino: u32,
            generation: u32,
        }
        const FILEID_INO32_GEN: i32 = 1;
        let mut changed = 0;
        for (&ino, sig) in then {
            let mut h = Handle { bytes: 8, kind: FILEID_INO32_GEN, ino, generation: (sig[3] >> 32) as u32 };
            let fd = unsafe {
                libc::open_by_handle_at(
                    std::os::fd::AsRawFd::as_raw_fd(&dir),
                    &mut h as *mut Handle as *mut libc::file_handle,
                    libc::O_PATH,
                )
            };
            if fd < 0 {
                changed += 1;
                continue;
            }
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let ok = unsafe { libc::fstat(fd, &mut st) } == 0;
            unsafe { libc::close(fd) };
            let same = ok
                && st.st_ctime as u64 == sig[0] >> 32
                && st.st_ctime_nsec as u64 == (sig[0] & 0xffff_ffff) >> 2
                && st.st_size as u64 == sig[2];
            if !same {
                changed += 1;
            }
        }
        Some(changed)
    }

    /// ext: block `k` is unchanged from the inodes; takes its digest from the previous backup.
    pub fn skip_block(&self, prev: &Digests, k: usize) -> bool {
        match self {
            Vol::Ext(x) if x.skip.get(k / 64).is_some_and(|w| w & (1 << (k % 64)) != 0) => match prev.table.get(k) {
                Some(&d) if d != 0 => {
                    x.table[k].store(d, Ordering::Relaxed);
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// For ext after `fill`: the digest of block `k` equals the previous backup's.
    pub fn unchanged(&self, prev: &Digests, k: usize) -> bool {
        match self {
            Vol::Btrfs { .. } => false,
            Vol::Ext(x) => prev.table.get(k).is_some_and(|&p| p != 0 && p == x.table[k].load(Ordering::Relaxed)),
        }
    }

    /// Digests to save as the base for the next incremental, tied to `owner` (see `backup::Table`).
    pub fn save_digests(&self, path: &std::path::Path, version_id: u64, partition_offset: u64, owner: &str) -> std::io::Result<()> {
        match self {
            Vol::Btrfs { prep, digests } => {
                digests.save(path, &CbtMeta::new(version_id, prep.fs.sb.generation, partition_offset, digests).owned(owner))
            }
            Vol::Ext(x) => {
                let d = Digests::from_table(x.table.iter().map(|a| a.load(Ordering::Relaxed)).collect());
                d.save(path, &CbtMeta::new(version_id, x.fs.wtime as u64, partition_offset, &d).owned(owner))?;
                match &x.inodes {
                    Some(i) => save_inodes(&path.with_extension("inodes"), version_id, &i.sigs),
                    None => {
                        let _ = std::fs::remove_file(path.with_extension("inodes"));
                        Ok(())
                    }
                }
            }
        }
    }

    /// Nothing more is read from the volume: a filesystem frozen for the upload is thawed now,
    /// not after the server has checked the version.
    pub fn release(&self) {
        if let Vol::Ext(x) = self
            && let Some(f) = x.freeze.lock().unwrap().take()
        {
            let mount = f.mount.clone();
            drop(f);
            eprintln!("{mount}: thawed after its last read");
        }
    }

    /// Btrfs subvolumes that could not be snapshotted (partial backup).
    pub fn incomplete(&self) -> bool {
        matches!(self, Vol::Btrfs { prep, .. } if !prep.unpinned.is_empty())
    }

    /// Removes the snapshots, unmounts, thaws (also done when dropped).
    pub fn cleanup(self) {
        if let Vol::Btrfs { mut prep, .. } = self {
            prep.release();
        }
    }
}

/// Inodes of the active swapfiles on the filesystem of `device` (from /proc/swaps).
fn swapfiles(device: &std::path::Path) -> Vec<u32> {
    let Ok(rdev) = std::fs::metadata(device).map(|m| m.rdev()) else { return Vec::new() };
    let swaps = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    swaps
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (path, kind) = (f.next()?, f.next()?);
            let m = std::fs::metadata(path.replace("\\040", " ")).ok()?;
            (kind == "file" && m.dev() == rdev).then_some(m.ino() as u32)
        })
        .collect()
}

/// Inode signatures of a backup, with its version: (inode, signature) little endian.
fn save_inodes(path: &std::path::Path, version_id: u64, sigs: &std::collections::HashMap<u32, [u64; 4]>) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(8 + sigs.len() * 36);
    out.extend(version_id.to_le_bytes());
    for (ino, s) in sigs {
        out.extend(ino.to_le_bytes());
        for v in s {
            out.extend(v.to_le_bytes());
        }
    }
    let raw = lz4_flex::block::compress_prepend_size(&out);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, raw)?;
    std::fs::rename(tmp, path)
}

/// The signatures saved by `save_inodes`, if they belong to `version_id`.
pub fn load_inodes(path: &std::path::Path, version_id: u64) -> Option<std::collections::HashMap<u32, [u64; 4]>> {
    let raw = lz4_flex::block::decompress_size_prepended(&std::fs::read(path).ok()?).ok()?;
    if raw.len() < 8 || u64::from_le_bytes(raw[..8].try_into().ok()?) != version_id || (raw.len() - 8) % 36 != 0 {
        return None;
    }
    let u64_at = |o: usize| u64::from_le_bytes(raw[o..o + 8].try_into().unwrap());
    Some(
        raw[8..]
            .chunks(36)
            .enumerate()
            .map(|(i, c)| {
                let o = 8 + i * 36;
                (u32::from_le_bytes(c[..4].try_into().unwrap()), [u64_at(o + 4), u64_at(o + 12), u64_at(o + 20), u64_at(o + 28)])
            })
            .collect(),
    )
}
