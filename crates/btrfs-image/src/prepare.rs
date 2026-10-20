//! Prepares a btrfs volume for backup: snapshots all subvolumes, reads the generation,
//! captures the unprotected blocks and builds the map of used blocks.

use crate::capture::{self, Capture, Pinned};
use crate::disk::u64_at;
use crate::snapshot;
use crate::exclude;
use crate::usedmap::{Kind, UsedMap};
use std::sync::Arc;
use crate::{Error, Filesystem};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The agent's snapshot directory, in the volume root (subvolid=5).
pub const SNAP_DIR: &str = ".abb-snapshots";
const TOPLEVEL_ID: u64 = 5;

pub struct Prepared {
    pub fs: Filesystem,
    pub cap: Capture,
    pub map: UsedMap,
    /// Mounted volume root (subvolid=5).
    pub top: PathBuf,
    /// Snapshot directory for this run.
    pub run_dir: PathBuf,
    /// Subvolumes that could not be snapshotted (e.g. active swapfile): their data in the image
    /// is not consistent.
    pub unpinned: Vec<PathBuf>,
    /// Subvolumes left out of the image (paths from the top level).
    pub excluded: Vec<PathBuf>,
    pub snapshot_time: Duration,
    mounted_by_us: bool,
}

/// Volume root, already mounted or mounted by us at `/run/abb-agent/top-<uuid>`.
pub fn top_mount(dev: &Path, uuid: &str) -> Result<(PathBuf, bool), Error> {
    let dev = std::fs::canonicalize(dev)?;
    let info = std::fs::read_to_string("/proc/self/mountinfo")?;
    for l in info.lines() {
        let Some((a, b)) = l.split_once(" - ") else { continue };
        let f: Vec<&str> = a.split(' ').collect();
        let g: Vec<&str> = b.split(' ').collect();
        if g.len() >= 2
            && g[0] == "btrfs"
            && f.len() >= 5
            && f[3] == "/"
            && std::fs::canonicalize(g[1]).is_ok_and(|p| p == dev)
        {
            return Ok((PathBuf::from(f[4].replace("\\040", " ")), false));
        }
    }
    let target = PathBuf::from(format!("/run/abb-agent/top-{uuid}"));
    std::fs::create_dir_all(&target)?;
    let c = |s: &str| CString::new(s).unwrap();
    let (src, tgt) = (c(&dev.to_string_lossy()), c(&target.to_string_lossy()));
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            tgt.as_ptr(),
            c("btrfs").as_ptr(),
            libc::MS_NOATIME,
            c("subvolid=5").as_ptr() as *const libc::c_void,
        )
    };
    if rc != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok((target, true))
}

/// `run_name`: name of this run's snapshot directory (e.g. "20261020T1530").
/// `exclude`: patterns of subvolume paths (from the top level) to leave out of the image.
/// `before_snapshot` is called with the subvolumes that will be left out, before the snapshots
/// are taken (so that what it writes ends up in the image).
pub fn prepare(
    dev: &Path,
    uuid: &str,
    run_name: &str,
    exclude: &[String],
    before_snapshot: &mut dyn FnMut(&[PathBuf]),
) -> Result<Prepared, Error> {
    let t = Instant::now();
    let (top, mounted_by_us) = top_mount(dev, uuid)?;
    if let Err(e) = snapshot::wait_deletions(&top, std::time::Duration::from_secs(30 * 60)) {
        if mounted_by_us {
            top_unmount(&top);
        }
        return Err(e);
    }
    let run_dir = top.join(SNAP_DIR).join(run_name);
    std::fs::create_dir_all(&run_dir)?;

    snapshot::sync(&top)?;
    let fs = Filesystem::open(dev)?;
    let subvols = snapshot::list_subvolumes(&fs, &top)?;
    let leaving: Vec<PathBuf> = exclude::matching(&subvols, Path::new(SNAP_DIR), exclude).into_iter().map(|s| s.path).collect();
    before_snapshot(&leaving);
    let mut sources: Vec<(u64, PathBuf, String)> = vec![(TOPLEVEL_ID, top.clone(), "5.toplevel".into())];
    for s in subvols.iter().filter(|s| !s.path.starts_with(SNAP_DIR)) {
        let name = format!("{}.{}", s.id, s.path.to_string_lossy().replace('/', "_"));
        sources.push((s.id, top.join(&s.path), name));
    }
    let mut unpinned_ids = Vec::new();
    let mut unpinned = Vec::new();
    for (id, path, name) in &sources {
        match snapshot::create_readonly(path, &run_dir, name) {
            Ok(()) => {}
            // A subvolume with an active swapfile cannot be snapshotted.
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                unpinned_ids.push(*id);
                unpinned.push(path.clone());
            }
            Err(e) => return Err(e),
        }
    }
    let snapshot_time = t.elapsed();

    // Generation just committed: from here on we work on it only.
    let mut fs = Filesystem::open(dev)?;
    let items = fs.root_items()?;
    let after = snapshot::list_subvolumes(&fs, &top)?;
    let run_prefix = Path::new(SNAP_DIR).join(run_name);
    let mut pinned = Vec::new();
    let mut snap_ids = Vec::new();
    let mut ours = Vec::new();
    for s in &after {
        if let Ok(rest) = s.path.strip_prefix(&run_prefix) {
            let src: u64 = rest
                .to_string_lossy()
                .split('.')
                .next()
                .and_then(|x| x.parse().ok())
                .ok_or_else(|| Error::Format(format!("snapshot with unexpected name: {}", s.path.display())))?;
            let otransid = items
                .iter()
                .find(|(i, _)| *i == s.id)
                .map(|(_, d)| u64_at(d, 303))
                .ok_or_else(|| Error::Format(format!("ROOT_ITEM of {} missing", s.id)))?;
            pinned.push(Pinned { subvol: src, snapshot_transid: otransid });
            snap_ids.push(s.id);
            ours.push(s.id);
        } else if s.path.starts_with(SNAP_DIR) {
            ours.push(s.id);
        }
    }
    let mut cap = capture::capture(&fs, &pinned, &ours, &unpinned_ids)?;
    // From here on the global trees (extent, csum, …) are read from the capture only.
    fs.use_capture(cap.nodes.clone());
    let mut map = UsedMap::build(&fs)?;
    // Data written while the snapshots were being taken may belong to a subvolume already
    // snapshotted and then be freed during the upload, whatever the trees say about it: every
    // data extent allocated since the first snapshot is captured too (a few MB at most).
    if let Some(first) = pinned.iter().map(|p| p.snapshot_transid).min() {
        let mut extra = 0u64;
        for r in map.ranges.iter().filter(|r| r.kind == Kind::Data && r.generation >= first) {
            if !cap.data.contains_key(&r.logical) {
                let mut buf = vec![0u8; r.len as usize];
                fs.read_at(&mut buf, r.physical)?;
                cap.data_bytes += r.len;
                cap.data.insert(r.logical, buf);
                extra += 1;
            }
        }
        let last = pinned.iter().map(|p| p.snapshot_transid).max().unwrap_or(first);
        eprintln!("snapshots in transactions {first}..{last}, generation {}: {extra} recent data extents captured", fs.sb.generation);
    }
    let mut excluded = Vec::new();
    if !exclude.is_empty() {
        let data = map.ranges.iter().filter(|r| r.kind == Kind::Data).map(|r| (r.logical, r.len));
        match exclude::plan(&fs, &after, &pinned, &snap_ids, Path::new(SNAP_DIR), exclude, data) {
            Ok(Some(ex)) => {
                // The edited nodes are sent instead of the ones on disk.
                let mut nodes = (*cap.nodes).clone();
                nodes.extend(ex.nodes.iter().map(|(k, v)| (*k, v.clone())));
                cap.nodes = Arc::new(nodes);
                fs.use_capture(cap.nodes.clone());
                map.apply_exclusion(&ex.data, &ex.nodes);
                excluded = ex.subvolumes;
            }
            Ok(None) => {}
            // Better a bigger backup than none: keep everything.
            Err(e) => eprintln!("cannot leave subvolumes out ({e}): they are backed up too"),
        }
    }
    Ok(Prepared { fs, cap, map, top, run_dir, unpinned, excluded, snapshot_time, mounted_by_us })
}

/// Deletes agent snapshots left over from previous runs: an interrupted backup, or a system
/// restored from an image (which contains the snapshots of the run it was taken in). Must be
/// called with no backup in progress. Returns how many snapshots were deleted.
pub fn remove_stale(dev: &Path, uuid: &str) -> Result<usize, Error> {
    let (top, mounted_by_us) = top_mount(dev, uuid)?;
    let base = top.join(SNAP_DIR);
    let mut removed = 0;
    let result = (|| -> Result<(), Error> {
        let Ok(runs) = std::fs::read_dir(&base) else { return Ok(()) };
        for run in runs {
            let run = run?.path();
            if !run.is_dir() {
                continue;
            }
            let mut names: Vec<String> = std::fs::read_dir(&run)?
                .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect();
            names.sort();
            for n in &names {
                snapshot::destroy(&run, n)?;
                removed += 1;
            }
            std::fs::remove_dir(&run)?;
        }
        let _ = std::fs::remove_dir(&base);
        Ok(())
    })();
    if mounted_by_us {
        let c = CString::new(top.to_string_lossy().as_bytes()).unwrap();
        unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
        let _ = std::fs::remove_dir(&top);
    }
    result.map(|_| removed)
}

/// Unmounts a top level mounted by [`top_mount`].
pub fn top_unmount(top: &Path) {
    let c = CString::new(top.to_string_lossy().as_bytes()).unwrap();
    unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
    let _ = std::fs::remove_dir(top);
}

impl Prepared {
    /// Deletes this run's snapshots and unmounts the root if we mounted it.
    pub fn cleanup(self) -> Result<(), Error> {
        let mut names: Vec<String> = std::fs::read_dir(&self.run_dir)?
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        names.sort();
        for n in &names {
            snapshot::destroy(&self.run_dir, n)?;
        }
        std::fs::remove_dir(&self.run_dir)?;
        let _ = std::fs::remove_dir(self.top.join(SNAP_DIR));
        if self.mounted_by_us {
            let c = CString::new(self.top.to_string_lossy().as_bytes()).unwrap();
            unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
            let _ = std::fs::remove_dir(&self.top);
        }
        Ok(())
    }
}
