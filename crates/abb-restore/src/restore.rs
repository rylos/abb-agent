//! Running a restore: download the image ranges of the plan, write them to the local disks,
//! then fix up partition tables, filesystem sizes and the EFI boot path.

use crate::gpt::{self, Gpt};
use crate::local::LocalDisk;
use crate::plan::{Mode, Place, Plan, used_end};
use crate::server::{Chunk, Range, Server};
use crate::spec::{Partition, Task, Version};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// `log.code` values of `check_complete` (synoabb::LogType).
const LOG_RESTORE_FINISH: u64 = 1203;
const LOG_RESTORE_CANCEL: u64 = 1202;
const LOG_INTERNAL_ERROR: u64 = 1000;

const MIB: u64 = 1 << 20;

pub enum Event {
    Step(String),
    /// Bytes of the ranges covered so far, and in total.
    Progress { done: u64, total: u64 },
    Warning(String),
}

/// Where the bytes of one range go.
pub(crate) struct Job {
    pub(crate) range: Range,
    /// Index in `devices`; None for data kept in memory (`table`).
    pub(crate) dev: Option<usize>,
    /// Device offset = image offset + delta.
    pub(crate) delta: i64,
    /// Original partition table, read to copy partition types and names.
    pub(crate) table: Option<Vec<u8>>,
    /// Further copies to write, as other deltas (the second stripe of btrfs DUP chunks).
    pub(crate) mirrors: Vec<i64>,
}

pub(crate) struct Device {
    pub(crate) path: PathBuf,
    pub(crate) file: File,
    /// Local disk index, if the device is a whole disk.
    disk: Option<usize>,
}

fn open_excl(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(path)
        .map_err(|e| format!("cannot open {} for writing: {e}", path.display()))
}

const BLKDISCARD: u32 = 0x1277;
const BLKRRPART: u32 = 0x125f;
const BLKFLSBUF: u32 = 0x1261;
const BLKSSZGET: u32 = 0x1268;
const BLKGETSIZE64: u32 = 0x80081272;

fn discard(f: &File, offset: u64, length: u64) {
    let range = [offset, length];
    unsafe { libc::ioctl(f.as_raw_fd(), BLKDISCARD as _, range.as_ptr()) };
}

/// The places where filesystems keep their signatures in the area at `offset`: its first MiB and
/// the btrfs superblock copies.
fn signature_areas(offset: u64, length: u64) -> Vec<(u64, u64)> {
    let mut out = vec![(offset, MIB.min(length))];
    for mirror in [64 * MIB, 256 << 30] {
        if mirror + 4096 <= length {
            out.push((offset + mirror, 4096));
        }
    }
    out
}

fn zero(f: &File, areas: &[(u64, u64)]) -> std::io::Result<()> {
    let zero = vec![0u8; MIB as usize];
    for &(mut o, len) in areas {
        let end = o + len;
        while o < end {
            let n = (end - o).min(MIB);
            f.write_all_at(&zero[..n as usize], o)?;
            o += n;
        }
    }
    Ok(())
}

/// Clears the places where filesystems keep their signatures, so that what was on the disk before
/// cannot be mistaken for the restored filesystem.
fn wipe_signatures(f: &File, offset: u64, length: u64) -> std::io::Result<()> {
    zero(f, &signature_areas(offset, length))
}

fn reread_partitions(f: &File) -> Result<(), String> {
    for _ in 0..10 {
        if unsafe { libc::ioctl(f.as_raw_fd(), BLKRRPART as _) } == 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("the kernel could not reread the partition table: {}", std::io::Error::last_os_error()))
}

fn wait_for(path: &Path) -> bool {
    for _ in 0..50 {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn device_size(f: &File) -> Option<u64> {
    let mut size: u64 = 0;
    (unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64 as _, &mut size) } == 0).then_some(size)
}

fn sector_size(f: &File) -> Option<u64> {
    let mut size: libc::c_int = 0;
    (unsafe { libc::ioctl(f.as_raw_fd(), BLKSSZGET as _, &mut size) } == 0).then_some(size as u64)
}

/// Is this (device number, size, logical sector size, serial or WWN) still the disk `l` that was
/// scanned? The kernel name of a disk can go to another one in between (unplugged, replugged).
fn same_disk(l: &LocalDisk, rdev: u64, size: Option<u64>, sector: Option<u64>, id: &str) -> Result<(), String> {
    let changed = if rdev != l.rdev {
        "device number"
    } else if size != Some(l.size) {
        "size"
    } else if sector != Some(l.sector_size) {
        "sector size"
    } else if id != l.id {
        "serial number"
    } else {
        return Ok(());
    };
    Err(format!("/dev/{} is not the disk scanned before (its {changed} changed): scan the disks again", l.name))
}

fn check_disk(f: &File, l: &LocalDisk) -> Result<(), String> {
    let rdev = f.metadata().map_err(|e| format!("/dev/{}: {e}", l.name))?.rdev();
    same_disk(l, rdev, device_size(f), sector_size(f), &crate::local::current_id(rdev))
}

/// Is the partition device opened at `path` partition `number` at `offset`, `length` bytes long?
fn check_partition(f: &File, path: &Path, number: u64, offset: u64, length: u64) -> Result<(), String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (start, n) = crate::local::partition_now(&name);
    if n != number || start != offset || device_size(f) != Some(length) {
        return Err(format!("{} is not the partition expected (number {number}, {length} bytes at {offset}): scan the disks again", path.display()));
    }
    Ok(())
}

/// Areas to zero if the restore does not finish, so that the incomplete devices are not taken
/// for restored ones: the partition tables of whole disks, the filesystem signatures of
/// partitions. Zeroed on drop too (a panic), unless [`Undo::finish`] was called.
#[derive(Default)]
struct Undo(Vec<(PathBuf, File, Areas)>);

impl Undo {
    /// Zeroes the areas if `failed`, and lets go of the devices. Returns what could not be zeroed.
    fn finish(&mut self, failed: bool) -> Vec<String> {
        let mut errs = Vec::new();
        for (path, f, areas) in self.0.drain(..) {
            if failed && let Err(e) = zero(&f, &areas).and_then(|_| f.sync_all()) {
                errs.push(format!("{}: {e}", path.display()));
            }
        }
        errs
    }

    /// The error of a restore that did not finish, with what it left on the disks.
    fn fail(&mut self, e: String) -> String {
        if self.0.is_empty() {
            return e;
        }
        let errs = self.finish(true);
        if errs.is_empty() {
            format!(
                "{e}\nThe restore is incomplete: the partition table of each whole disk restored and the signature of each volume restored into a partition have been cleared, so that they are not taken for restored ones. Run the restore again."
            )
        } else {
            format!("{e}\nThe disks are incomplete, and clearing them failed ({}): do not use them before the restore is run again.", errs.join("; "))
        }
    }
}

impl Drop for Undo {
    fn drop(&mut self) {
        self.finish(true);
    }
}

pub fn run(
    server: &mut Server,
    task: &Task,
    v: &Version,
    plan: &Plan,
    locals: &[LocalDisk],
    cancel: &AtomicBool,
    report: &mut dyn FnMut(Event),
) -> Result<(), String> {
    let Layout { mut jobs, targets, tables } = layout(v, plan, locals)?;
    let specials = if plan.exclude_subvolumes.is_empty() { vec![] } else { carve(&mut jobs, v) };
    let mode = plan.mode.code();
    let start = |s: &mut Server, ranges: &[Range]| s.start(task.device_id, task.task_id, v.id, mode, &crate::local::machine_uuid(), ranges);
    // Nothing is written before the server has accepted the restore: the first restore task is
    // created now, the others (more shares, btrfs passes) as they come.
    report(Event::Step("Starting the restore on the server".into()));
    let (shares, total) = deal(std::mem::take(&mut jobs));
    let first = match shares.first() {
        Some(share) => {
            let ranges: Vec<Range> = share.iter().map(|j| j.range.clone()).collect();
            Some(start(server, &ranges).map_err(|e| format!("the server refused the restore: {e}"))?)
        }
        None => {
            check_version(server, task, v)?;
            None
        }
    };
    report(Event::Step("Preparing the disks".into()));
    let mut undo = Undo::default();
    let devices = match open_targets(&targets, &tables, locals, &mut undo) {
        Ok(d) => d,
        Err(e) => {
            if let Some(id) = first {
                let _ = server.finish(id, "error", LOG_INTERNAL_ERROR);
            }
            return Err(undo.fail(e));
        }
    };
    report(Event::Step("Restoring".into()));
    let result = run_shares(server, &start, shares, total, first, &devices, cancel, report).map(|j| jobs = j);
    let mut recreate = Vec::new();
    let result = result.and_then(|_| {
        for sp in &specials {
            let ctx = crate::btrfs::Ctx { server: &mut *server, task, version: v, mode: plan.mode.code(), devices: &devices, cancel };
            let left = crate::btrfs::restore(ctx, sp, &plan.exclude_subvolumes, report)?;
            if !left.deleted.is_empty() {
                recreate.push((sp.disk, sp.part.offset, sp.dev, left));
            }
        }
        Ok(())
    });
    let result = result.and_then(|_| {
        // The start of the disks last: until now they show no partition table.
        for j in &jobs {
            if let (Some(d), Some(head)) = (j.dev, &j.table) {
                let d = &devices[d];
                d.file.write_all_at(head, (j.range.offset as i64 + j.delta) as u64).map_err(|e| format!("disk: {}: {e}", d.path.display()))?;
            }
        }
        for d in &devices {
            d.file.sync_all().map_err(|e| format!("disk: {}: {e}", d.path.display()))?;
        }
        report(Event::Step("Fixing up partition tables".into()));
        finish_disks(v, plan, locals, &jobs, &devices, report)
    });
    let result = match result {
        Ok(work) => {
            undo.finish(false);
            Ok(work)
        }
        Err(e) => Err(undo.fail(e)),
    };
    // Mounting and checking need the devices that are open exclusively here.
    // Where the kernel will show the partitions holding subvolumes to recreate.
    let recreate: Vec<(PathBuf, crate::btrfs::Left)> = recreate
        .into_iter()
        .filter_map(|(disk, offset, dev, paths)| partition_device(v, locals, &devices, disk, offset, dev).map(|p| (p, paths)))
        .collect();
    drop(devices);
    result.map(|mut work| {
        work.recreate = recreate;
        after_restore(plan, work, report)
    })
}

/// Checks that the server still has version `v` (and takes the password), for a restore whose
/// first restore task is created only after the disks are prepared.
fn check_version(server: &mut Server, task: &Task, v: &Version) -> Result<(), String> {
    let resp = server.versions(task.device_id, task.task_id).map_err(|e| format!("the server refused the restore: {e}"))?;
    if !Version::list(&resp).iter().any(|o| o.id == v.id) {
        return Err(format!("the server no longer has version {}", v.id));
    }
    Ok(())
}

/// A btrfs partition restored in phases, to leave subvolumes out.
pub(crate) struct Special {
    /// Disk of the version.
    pub(crate) disk: usize,
    pub(crate) part: Partition,
    /// Device index and delta (device offset = image offset + delta), as for its job.
    pub(crate) dev: usize,
    pub(crate) delta: i64,
}

/// Takes the btrfs partitions out of the jobs (splitting the jobs around them).
fn carve(jobs: &mut Vec<Job>, v: &Version) -> Vec<Special> {
    let mut specials = Vec::new();
    let mut out = Vec::new();
    for job in jobs.drain(..) {
        // Jobs kept in memory (the start of a disk) hold no partition.
        let (Some(dev), None) = (job.dev, &job.table) else {
            out.push(job);
            continue;
        };
        let Some(disk) = v.disks.iter().position(|d| d.image_path == job.range.image_path) else {
            out.push(job);
            continue;
        };
        let (start, end) = (job.range.offset, job.range.offset + job.range.length);
        let mut parts: Vec<&Partition> = v.disks[disk]
            .partitions
            .iter()
            .filter(|p| p.fs == "btrfs" && !p.excluded && p.offset >= start && p.offset + p.length <= end)
            .collect();
        parts.sort_by_key(|p| p.offset);
        let mut pos = start;
        for p in parts {
            if p.offset > pos {
                let mut r = job.range.clone();
                (r.offset, r.length) = (pos, p.offset - pos);
                out.push(Job { range: r, dev: Some(dev), delta: job.delta, table: None, mirrors: vec![] });
            }
            specials.push(Special { disk, part: p.clone(), dev, delta: job.delta });
            pos = p.offset + p.length;
        }
        if pos < end {
            let mut r = job.range.clone();
            (r.offset, r.length) = (pos, end - pos);
            out.push(Job { range: r, dev: Some(dev), delta: job.delta, table: None, mirrors: vec![] });
        }
    }
    *jobs = out;
    specials
}

/// Device node of a restored partition (after the partition table is reread).
fn partition_device(
    v: &Version,
    locals: &[LocalDisk],
    devices: &[Device],
    disk: usize,
    offset: u64,
    dev: usize,
) -> Option<PathBuf> {
    let d = &devices[dev];
    let Some(j) = d.disk else { return Some(d.path.clone()) };
    let l = &locals[j];
    // Same position as in the backup: device and system modes keep the layout.
    let n = v.disks[disk].partitions.iter().find(|p| p.offset == offset)?.number;
    Some(l.partition_path(n))
}

/// (offset, length) pairs, in bytes.
type Areas = Vec<(u64, u64)>;
/// Volumes mode: a local disk, its new partition table, and the new partitions in it.
type NewTable = (usize, Gpt, Areas);

/// A device to write to, and how to make it ready.
struct Target {
    local: usize,
    /// For a partition device: its number, offset and length on the disk (checked when opened).
    partition: Option<(u64, u64, u64)>,
    /// Areas (offset, length) to discard, and to clear of filesystem signatures, before writing;
    /// a length of `u64::MAX` goes to the end of the device.
    discard: Vec<(u64, u64)>,
    wipe: Vec<(u64, u64)>,
    /// Areas to zero before writing: the partition tables there before, and where the image's
    /// table goes (written last).
    zero: Vec<(u64, u64)>,
    /// Areas to zero if the restore does not finish (see [`Undo`]).
    undo: Vec<(u64, u64)>,
}

/// What a plan writes, worked out before anything is.
struct Layout {
    jobs: Vec<Job>,
    /// In the order of the devices the jobs refer to.
    targets: Vec<Target>,
    tables: Vec<NewTable>,
}

/// Lists the ranges to download and the devices they go to.
fn layout(v: &Version, plan: &Plan, locals: &[LocalDisk]) -> Result<Layout, String> {
    let mut jobs = Vec::new();
    let mut targets: Vec<Target> = Vec::new();
    let mut tables: Vec<NewTable> = Vec::new();
    let range = |d: &crate::spec::Disk, p: Option<&Partition>, offset: u64, length: u64| Range {
        image_path: d.image_path.clone(),
        object_id: d.object_id,
        disk_number: d.number + 1,
        partition_number: p.map_or(0, |p| p.number),
        alias_name: p.map_or(String::new(), |p| p.label.clone()),
        offset,
        length,
        done: 0,
    };
    let job = |range: Range, dev: usize, delta: i64| Job { range, dev: Some(dev), delta, table: None, mirrors: vec![] };
    // The start of a disk, held in memory and written at the end.
    let head = |range: Range, dev: usize| Job { table: Some(vec![0; range.length as usize]), range, dev: Some(dev), delta: 0, mirrors: vec![] };
    match plan.mode {
        Mode::Device | Mode::System => {
            for (i, m) in plan.disks.iter().enumerate() {
                let Some(j) = *m else { continue };
                let (d, l) = (&v.disks[i], &locals[j]);
                let dev = targets.len();
                let s = l.sector_size;
                // Primary and backup tables, also where the backup of a smaller image would be.
                let tables_at = {
                    let mut t = vec![(0, 34 * s), (l.size.saturating_sub(33 * s), 33 * s)];
                    if d.size < l.size && d.size >= 33 * s {
                        t.push((d.size - 33 * s, 33 * s));
                    }
                    t
                };
                let first = d.partitions.first().map_or(MIB, |p| p.offset).min(MIB);
                if plan.mode == Mode::Device {
                    // On a smaller disk, up to the end of the last partition (`disk_fits`); never
                    // past the end of the disk.
                    let length = if l.size >= d.size { d.size } else { used_end(d).div_ceil(MIB) * MIB }.min(l.size);
                    let first = first.min(length);
                    jobs.push(head(range(d, None, 0, first), dev));
                    if length > first {
                        jobs.push(job(range(d, None, first, length - first), dev, 0));
                    }
                    let wipe = d.partitions.iter().map(|p| (p.offset, p.length)).collect();
                    let mut zero = tables_at.clone();
                    zero.push((0, first));
                    targets.push(Target { local: j, partition: None, discard: vec![(0, l.size)], wipe, zero, undo: tables_at });
                } else {
                    let mut t = Target { local: j, partition: None, discard: vec![], wipe: vec![], zero: vec![], undo: vec![] };
                    if l.partitions.is_empty() {
                        jobs.push(head(range(d, None, 0, first), dev));
                        // Up to the first partition (a boot loader may live in the gap).
                        let p0 = d.partitions.first().map_or(first, |p| p.offset).min(l.size);
                        if p0 > first {
                            jobs.push(job(range(d, None, first, p0 - first), dev, 0));
                        }
                        t.zero = tables_at.clone();
                        t.zero.push((0, p0.max(first)));
                        t.undo = tables_at;
                    }
                    for p in d.partitions.iter().filter(|p| p.is_system() && !p.excluded) {
                        t.discard.push((p.offset, p.length));
                        t.wipe.push((p.offset, p.length));
                        t.undo.extend(signature_areas(p.offset, p.length));
                        jobs.push(job(range(d, Some(p), p.offset, p.length), dev, 0));
                    }
                    targets.push(t);
                }
            }
        }
        Mode::Volumes => {
            // New partitions first, so that each volume is written where it will stay.
            for t in &plan.volumes {
                let Some(Place::Free(start)) = t.place else { continue };
                let (p, l) = (&v.disks[t.disk].partitions[t.part], &locals[t.local]);
                if !tables.iter().any(|(k, _, _)| *k == t.local) {
                    tables.push((t.local, l.gpt.clone().unwrap_or_else(|| Gpt::new(l.sector_size)), vec![]));
                }
                let (_, g, new) = tables.iter_mut().find(|(k, _, _)| *k == t.local).unwrap();
                let slot = g.entries.iter().position(|e| !e.is_used()).ok_or("partition table full")?;
                let sectors = p.length.div_ceil(l.sector_size);
                let type_guid = if p.is_esp() {
                    gpt::ESP
                } else if p.is_bios_boot() {
                    gpt::BIOS_BOOT
                } else {
                    gpt::LINUX_FS
                };
                g.entries[slot] = gpt::Entry {
                    type_guid,
                    guid: gpt::random_guid(),
                    first_lba: start,
                    last_lba: start + sectors - 1,
                    attributes: 0,
                    name: vec![0; 72],
                };
                new.push((start * l.sector_size, sectors * l.sector_size));
            }
            let mut sources: Vec<usize> = Vec::new();
            for t in &plan.volumes {
                let (d, p, l) = (&v.disks[t.disk], &v.disks[t.disk].partitions[t.part], &locals[t.local]);
                let partition = match t.place {
                    Some(Place::Partition(n)) => {
                        let q = l.partitions.iter().find(|q| q.number == n).ok_or_else(|| format!("/dev/{}: no partition {n}", l.name))?;
                        (n, q.offset, q.length)
                    }
                    Some(Place::Free(start)) => {
                        let (_, g, _) = tables.iter().find(|(k, _, _)| *k == t.local).unwrap();
                        let (n, e) = g.used().find(|(_, e)| e.first_lba == start).unwrap();
                        (n, start * l.sector_size, (e.last_lba - e.first_lba + 1) * l.sector_size)
                    }
                    None => return Err(format!("{}: no target chosen", p.label)),
                };
                let dev = targets.len();
                targets.push(Target {
                    local: t.local,
                    partition: Some(partition),
                    discard: vec![(0, u64::MAX)],
                    wipe: vec![(0, u64::MAX)],
                    zero: vec![],
                    undo: signature_areas(0, partition.2),
                });
                jobs.push(job(range(d, Some(p), p.offset, p.length), dev, -(p.offset as i64)));
                if !sources.contains(&t.disk) {
                    sources.push(t.disk);
                }
            }
            // The original partition tables, to give new partitions their type and name.
            for i in sources {
                let d = &v.disks[i];
                if d.gpt {
                    let len = d.partitions.first().map_or(MIB, |p| p.offset).min(MIB);
                    jobs.push(Job { range: range(d, None, 0, len), dev: None, delta: 0, table: Some(vec![0; len as usize]), mirrors: vec![] });
                }
            }
        }
    }
    Ok(Layout { jobs, targets, tables })
}

/// Opens the target devices, making sure they are the disks and partitions scanned, and clears
/// them: old signatures and partition tables. Writes the new partition tables of volumes mode.
fn open_targets(targets: &[Target], tables: &[NewTable], locals: &[LocalDisk], undo: &mut Undo) -> Result<Vec<Device>, String> {
    // Every disk is checked before anything is written to any of them.
    let mut disks: Vec<usize> = targets.iter().map(|t| t.local).chain(tables.iter().map(|t| t.0)).collect();
    disks.sort_unstable();
    disks.dedup();
    for &j in &disks {
        let l = &locals[j];
        let f = File::open(l.path()).map_err(|e| format!("cannot open /dev/{}: {e}", l.name))?;
        check_disk(&f, l)?;
    }
    for (k, g, new) in tables {
        let l = &locals[*k];
        let file = open_excl(&l.path())?;
        check_disk(&file, l)?;
        let err = |e: std::io::Error| format!("disk: /dev/{}: {e}", l.name);
        // Nothing of what the free space held may show up in the new partitions. (Not a clone of
        // `file` to undo: the exclusive claim would keep the partitions from being opened.)
        let shared = File::options().read(true).write(true).open(l.path()).map_err(err)?;
        for &(o, len) in new {
            undo.0.push((l.path(), shared.try_clone().map_err(err)?, signature_areas(o, len)));
            wipe_signatures(&file, o, len).map_err(err)?;
        }
        g.write(&file, l.size, l.gpt.is_none()).map_err(err)?;
        reread_partitions(&file)?;
    }
    let mut devices = Vec::new();
    for t in targets {
        let l = &locals[t.local];
        let path = match t.partition {
            Some((n, _, _)) => l.partition_path(n),
            None => l.path(),
        };
        if !wait_for(&path) {
            return Err(format!("{} did not appear", path.display()));
        }
        let file = open_excl(&path)?;
        match t.partition {
            Some((n, offset, length)) => check_partition(&file, &path, n, offset, length)?,
            None => check_disk(&file, l)?,
        }
        let size = device_size(&file).ok_or_else(|| format!("{}: size unknown", path.display()))?;
        let clamp = |areas: &[(u64, u64)]| -> Vec<(u64, u64)> {
            areas.iter().filter(|a| a.0 < size).map(|&(o, len)| (o, len.min(size - o))).collect()
        };
        let err = |e: std::io::Error| format!("disk: {}: {e}", path.display());
        undo.0.push((path.clone(), file.try_clone().map_err(err)?, clamp(&t.undo)));
        for (o, len) in clamp(&t.discard) {
            discard(&file, o, len);
        }
        for (o, len) in clamp(&t.wipe) {
            wipe_signatures(&file, o, len).map_err(err)?;
        }
        zero(&file, &clamp(&t.zero)).map_err(err)?;
        devices.push(Device { path, file, disk: t.partition.is_none().then_some(t.local) });
    }
    Ok(devices)
}

/// Parallel downloads: the server reads one restore task at a time well below what its disks
/// can give (measured on a DS with HDDs: 8 tasks at once more than double the throughput).
const STREAMS: usize = 8;
/// Restore tasks per transfer, to balance the streams; more when the ranges need it.
const TASKS: usize = 2 * STREAMS;
/// Ranges per restore task.
const MAX_RANGES: usize = 2000;
/// Longer ranges are cut, so that they spread over the streams too.
const SPLIT: u64 = 256 * MIB;

/// Downloads `jobs` with up to [`STREAMS`] restore tasks at once, each over a share of the
/// ranges. `start` creates a restore task on the server for some ranges. Returns the jobs, cut
/// where they were long, with their tables filled.
pub(crate) fn transfer(
    server: &Server,
    start: &(dyn Fn(&mut Server, &[Range]) -> abb_proto::transport::Result<u64> + Sync),
    jobs: Vec<Job>,
    devices: &[Device],
    cancel: &AtomicBool,
    report: &mut dyn FnMut(Event),
) -> Result<Vec<Job>, String> {
    let (shares, total) = deal(jobs);
    run_shares(server, start, shares, total, None, devices, cancel, report)
}

/// Cuts the long jobs and deals them into the shares of the restore tasks. Returns the shares
/// and the bytes they cover.
fn deal(jobs: Vec<Job>) -> (Vec<Vec<Job>>, u64) {
    let mut cut = Vec::with_capacity(jobs.len());
    for j in jobs {
        if j.table.is_some() || j.range.length <= SPLIT {
            cut.push(j);
            continue;
        }
        let mut o = 0;
        while o < j.range.length {
            let mut range = j.range.clone();
            range.offset += o;
            range.length = SPLIT.min(j.range.length - o);
            cut.push(Job { range, dev: j.dev, delta: j.delta, table: None, mirrors: j.mirrors.clone() });
            o += SPLIT;
        }
    }
    cut.sort_by(|a, b| (&a.range.image_path, a.range.offset).cmp(&(&b.range.image_path, b.range.offset)));
    let total: u64 = cut.iter().map(|j| j.range.length).sum();
    // Ranges dealt in turn, so that every task gets some of each part of the disk.
    let tasks = TASKS.max(cut.len().div_ceil(MAX_RANGES)).min(cut.len());
    let mut shares: Vec<Vec<Job>> = (0..tasks).map(|_| Vec::new()).collect();
    for (i, j) in cut.into_iter().enumerate() {
        shares[i % tasks].push(j);
    }
    (shares, total)
}

/// Downloads the shares, one restore task each. `first` is the restore task already created for
/// the first share, if any.
#[allow(clippy::too_many_arguments)]
fn run_shares(
    server: &Server,
    start: &(dyn Fn(&mut Server, &[Range]) -> abb_proto::transport::Result<u64> + Sync),
    shares: Vec<Vec<Job>>,
    total: u64,
    first: Option<u64>,
    devices: &[Device],
    cancel: &AtomicBool,
    report: &mut dyn FnMut(Event),
) -> Result<Vec<Job>, String> {
    let shares: Vec<std::sync::Mutex<Vec<Job>>> = shares.into_iter().map(std::sync::Mutex::new).collect();
    let first = std::sync::Mutex::new(first);
    let next = AtomicUsize::new(0);
    let done = AtomicU64::new(0);
    let running_streams = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let errors = std::sync::Mutex::new(Vec::<String>::new());
    let warnings = std::sync::Mutex::new(Vec::<String>::new());
    let streams = STREAMS.min(shares.len());
    std::thread::scope(|sc| {
        for _ in 0..streams {
            let mut s = server.clone();
            running_streams.fetch_add(1, Ordering::SeqCst);
            let (shares, first, next, done, failed, errors, warnings, running_streams) =
                (&shares, &first, &next, &done, &failed, &errors, &warnings, &running_streams);
            sc.spawn(move || {
                while !failed.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(share) = shares.get(i) else { break };
                    let mut share = share.lock().unwrap();
                    let ranges: Vec<Range> = share.iter().map(|j| j.range.clone()).collect();
                    let pre = if i == 0 { first.lock().unwrap().take() } else { None };
                    let started = match pre {
                        Some(id) => Ok(id),
                        None => start(&mut s, &ranges).map_err(|e| format!("the server refused the restore: {e}")),
                    };
                    let res = started.and_then(|running| {
                        let res = download(&mut s, running, &mut share, devices, done, cancel, failed, &|w| warnings.lock().unwrap().push(w));
                        let (status, code) = match &res {
                            Ok(()) => ("complete", LOG_RESTORE_FINISH),
                            Err(_) if cancel.load(Ordering::Relaxed) => ("cancel", LOG_RESTORE_CANCEL),
                            Err(_) => ("error", LOG_INTERNAL_ERROR),
                        };
                        let _ = s.finish(running, status, code);
                        res
                    });
                    if let Err(e) = res {
                        // The error first: the other streams stop on `failed` with errors of
                        // their own, not the cause.
                        errors.lock().unwrap().push(e);
                        failed.store(true, Ordering::Relaxed);
                    }
                }
                running_streams.fetch_sub(1, Ordering::SeqCst);
            });
        }
        let mut shown = u64::MAX;
        loop {
            for w in warnings.lock().unwrap().drain(..) {
                report(Event::Warning(w));
            }
            let d = done.load(Ordering::Relaxed);
            if d != shown {
                shown = d;
                report(Event::Progress { done: d, total });
            }
            if running_streams.load(Ordering::SeqCst) == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    // The first task, if no stream got to it.
    if let Some(id) = first.into_inner().unwrap() {
        let (status, code) = if cancel.load(Ordering::Relaxed) { ("cancel", LOG_RESTORE_CANCEL) } else { ("error", LOG_INTERNAL_ERROR) };
        let _ = server.clone().finish(id, status, code);
    }
    if let Some(e) = errors.into_inner().unwrap().into_iter().next() {
        return Err(e);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    Ok(shares.into_iter().flat_map(|s| s.into_inner().unwrap()).collect())
}

/// Downloads the ranges of one restore task into their jobs; `done` counts the bytes written.
/// Stops when `cancel` or `stop` (another stream failed) is set.
#[allow(clippy::too_many_arguments)]
fn download(
    server: &mut Server,
    running: u64,
    jobs: &mut [Job],
    devices: &[Device],
    done: &AtomicU64,
    cancel: &AtomicBool,
    stop: &AtomicBool,
    warn: &dyn Fn(String),
) -> Result<(), String> {
    // Jobs in order of image and offset, to find the one a chunk belongs to by bisection.
    jobs.sort_by(|a, b| (&a.range.image_path, a.range.offset).cmp(&(&b.range.image_path, b.range.offset)));
    let find = |jobs: &[Job], path: &str, offset: u64| {
        let i = jobs.partition_point(|j| (j.range.image_path.as_str(), j.range.offset) <= (path, offset));
        i.checked_sub(1).filter(|&i| jobs[i].range.image_path == path)
    };
    let mut attempt = 0;
    // A write to a local disk that failed: not a network error, not retried.
    let mut disk_error = None;
    loop {
        let ranges: Vec<Range> = jobs.iter().map(|j| j.range.clone()).collect();
        let res = server.download(running, &ranges, |c| {
            if cancel.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("cancelled"));
            }
            if stop.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("stopped after an error elsewhere"));
            }
            match c {
                Chunk::Data { image_path, offset, data } => {
                    let job = find(jobs, &image_path, offset)
                        .map(|i| &mut jobs[i])
                        .filter(|j| offset < j.range.offset + j.range.length)
                        .ok_or_else(|| std::io::Error::other(format!("{image_path} offset {offset} outside the requested ranges")))?;
                    let end = (offset + data.len() as u64).min(job.range.offset + job.range.length);
                    let data = &data[..(end - offset) as usize];
                    if let Some(t) = &mut job.table {
                        let s = (offset - job.range.offset) as usize;
                        t[s..s + data.len()].copy_from_slice(data);
                    } else if let Some(d) = job.dev {
                        for delta in std::iter::once(job.delta).chain(job.mirrors.iter().copied()) {
                            let at = (offset as i64 + delta) as u64;
                            if let Err(e) = devices[d].file.write_all_at(data, at) {
                                let e = format!("disk: {} at {at}: {e}", devices[d].path.display());
                                disk_error = Some(e.clone());
                                return Err(std::io::Error::other(e));
                            }
                        }
                    }
                    let now = end - job.range.offset;
                    done.fetch_add(now.saturating_sub(job.range.done), Ordering::Relaxed);
                    job.range.done = job.range.done.max(now);
                }
                Chunk::EndOfRange { image_path, offset } => {
                    // The range that ends here starts at or before offset - 1.
                    if let Some(j) = offset.checked_sub(1).and_then(|o| find(jobs, &image_path, o)).map(|i| &mut jobs[i])
                        && j.range.offset + j.range.length == offset
                    {
                        done.fetch_add(j.range.length - j.range.done, Ordering::Relaxed);
                        j.range.done = j.range.length;
                    }
                }
                Chunk::Done => {}
            }
            Ok(())
        });
        if let Some(e) = disk_error.take() {
            return Err(e);
        }
        match res {
            Ok(()) => {
                if let Some(j) = jobs.iter().find(|j| j.range.done < j.range.length) {
                    return Err(format!("the server ended the transfer before the end of {}", j.range.image_path));
                }
                return Ok(());
            }
            Err(e) if cancel.load(Ordering::Relaxed) => return Err(format!("cancelled ({e})")),
            Err(e) if stop.load(Ordering::Relaxed) => return Err(e.to_string()),
            Err(abb_proto::Error::Io(e)) if attempt < 10 && e.kind() != std::io::ErrorKind::Other => {
                attempt += 1;
                warn(format!("connection lost ({e}), resuming (attempt {attempt})"));
                std::thread::sleep(Duration::from_secs((1u64 << attempt).min(30)));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// What is left once the disks are closed.
#[derive(Default)]
struct AfterWork {
    /// Disks whose partition table must be reread by the kernel.
    reread: Vec<PathBuf>,
    /// Filesystems to grow: device and type.
    grow: Vec<(PathBuf, String)>,
    /// EFI system partitions to check.
    esps: Vec<PathBuf>,
    /// btrfs partitions with subvolumes left out at restore time.
    recreate: Vec<(PathBuf, crate::btrfs::Left)>,
}

/// After the data: partition tables for the real disk size, bigger partitions and filesystems,
/// EFI fallback boot path.
fn finish_disks(
    v: &Version,
    plan: &Plan,
    locals: &[LocalDisk],
    jobs: &[Job],
    devices: &[Device],
    report: &mut dyn FnMut(Event),
) -> Result<AfterWork, String> {
    let mut work = AfterWork::default();
    let (grown, esps) = (&mut work.grow, &mut work.esps);
    match plan.mode {
        Mode::Device | Mode::System => {
            for dev in devices {
                let Some(j) = dev.disk else { continue };
                let l = &locals[j];
                let i = plan.disks.iter().position(|m| *m == Some(j)).unwrap();
                let d = &v.disks[i];
                if !d.gpt {
                    continue;
                }
                if plan.mode == Mode::System && !l.partitions.is_empty() {
                    // The local table was kept as it was.
                    for p in d.partitions.iter().filter(|p| p.is_system() && !p.excluded) {
                        let Some(q) = l.partitions.iter().find(|q| q.offset == p.offset) else { continue };
                        if p.is_esp() {
                            esps.push(l.partition_path(q.number));
                        }
                        if plan.grow && q.length > p.length && growable(p) {
                            grown.push((l.partition_path(q.number), p.fs.clone()));
                        }
                    }
                    continue;
                }
                let mut g = Gpt::read(&dev.file, l.sector_size)
                    .map_err(|e| format!("disk: {}: {e}", l.name))?
                    .ok_or_else(|| format!("{}: the restored partition table is not valid", l.name))?;
                // The source disk still attached: two disks with the same GUIDs.
                for o in locals.iter().filter(|o| o.name != l.name && o.gpt.as_ref().is_some_and(|og| og.disk_guid == g.disk_guid)) {
                    report(Event::Warning(format!(
                        "/dev/{} has the same disk GUID as the restored /dev/{} (the original disk?): their partition UUIDs are the same too, detach it before booting",
                        o.name, l.name
                    )));
                }
                let mut grow_part = None;
                if plan.grow && plan.mode == Mode::Device && l.size > d.size {
                    let last_usable = g.last_usable(l.size);
                    if let Some((n, _)) = g.used().max_by_key(|(_, e)| e.last_lba) {
                        let e = &mut g.entries[n as usize - 1];
                        if let Some(p) = d.partitions.iter().find(|p| p.offset == e.first_lba * l.sector_size)
                            && !p.excluded
                            && growable(p)
                        {
                            e.last_lba = last_usable;
                            grow_part = Some((n, p.fs.clone()));
                        }
                    }
                }
                g.fit(l.size);
                g.write(&dev.file, l.size, false).map_err(|e| format!("disk: {}: {e}", l.name))?;
                // The table is on the disk (synced): a cache left behind only delays the reread.
                if unsafe { libc::ioctl(dev.file.as_raw_fd(), BLKFLSBUF as _) } != 0 {
                    report(Event::Warning(format!("/dev/{}: cannot flush the buffer cache: {}", l.name, std::io::Error::last_os_error())));
                }
                if let Err(e) = reread_partitions(&dev.file) {
                    report(Event::Warning(e));
                }
                if let Some((n, fs)) = grow_part {
                    grown.push((l.partition_path(n), fs));
                }
                for (n, e) in g.used() {
                    if e.type_guid == gpt::ESP {
                        esps.push(l.partition_path(n));
                    }
                }
            }
        }
        Mode::Volumes => {
            for t in &plan.volumes {
                let (d, p, l) = (&v.disks[t.disk], &v.disks[t.disk].partitions[t.part], &locals[t.local]);
                let start = match t.place {
                    None => continue,
                    Some(Place::Free(start)) => start,
                    Some(Place::Partition(n)) => {
                        let bigger = l.partitions.iter().any(|q| q.number == n && q.length > p.length);
                        if plan.grow && bigger && growable(p) {
                            grown.push((l.partition_path(n), p.fs.clone()));
                        }
                        continue;
                    }
                };
                // Give the new partition the type, name and attributes of the original one.
                let Some(src) = jobs
                    .iter()
                    .find(|j| j.dev.is_none() && j.table.is_some() && j.range.image_path == d.image_path)
                    .and_then(|j| Gpt::parse(j.table.as_ref().unwrap(), d.sector_size))
                else {
                    continue;
                };
                let Some((_, orig)) = src.used().find(|(_, e)| e.first_lba * d.sector_size == p.offset) else { continue };
                let file = File::options().read(true).write(true).open(l.path()).map_err(|e| format!("{}: {e}", l.name))?;
                check_disk(&file, l)?;
                let mut g = Gpt::read(&file, l.sector_size).map_err(|e| format!("disk: {}: {e}", l.name))?.ok_or("partition table lost")?;
                if let Some((n, e)) = g.entries.iter_mut().enumerate().find(|(_, e)| e.is_used() && e.first_lba == start) {
                    e.type_guid = orig.type_guid;
                    e.name = orig.name.clone();
                    e.attributes = orig.attributes;
                    // Keep the partition UUID (boot entries and fstab may use it) unless another
                    // disk already has it.
                    if !locals.iter().any(|o| o.gpt.as_ref().is_some_and(|og| og.used().any(|(_, x)| x.guid == orig.guid))) {
                        e.guid = orig.guid;
                    }
                    if e.type_guid == gpt::ESP {
                        esps.push(l.partition_path(n as u64 + 1));
                    }
                    if plan.grow && (e.last_lba - e.first_lba + 1) * l.sector_size > p.length && growable(p) {
                        grown.push((l.partition_path(n as u64 + 1), p.fs.clone()));
                    }
                }
                g.write(&file, l.size, false).map_err(|e| format!("disk: {}: {e}", l.name))?;
                if !work.reread.contains(&l.path()) {
                    work.reread.push(l.path());
                }
            }
        }
    }
    Ok(work)
}

fn growable(p: &Partition) -> bool {
    p.fs.starts_with("ext") || p.fs == "btrfs"
}

/// Partition tables, growing filesystems and the EFI boot path, once the disks are closed.
fn after_restore(plan: &Plan, work: AfterWork, report: &mut dyn FnMut(Event)) {
    for disk in &work.reread {
        match File::open(disk) {
            Ok(f) => {
                if let Err(e) = reread_partitions(&f) {
                    report(Event::Warning(format!("{}: {e}", disk.display())));
                }
            }
            Err(e) => report(Event::Warning(format!("{}: {e}", disk.display()))),
        }
    }
    for (path, fs) in work.grow {
        report(Event::Step(format!("Growing {fs} on {}", path.display())));
        if !wait_for(&path) {
            report(Event::Warning(format!("{} did not appear: filesystem not grown", path.display())));
            continue;
        }
        if let Err(e) = crate::fs::grow(&path, &fs) {
            report(Event::Warning(format!("{}: filesystem not grown: {e}", path.display())));
        }
    }
    for (dev, left) in &work.recreate {
        if !wait_for(dev) {
            report(Event::Warning(format!("{} did not appear: subvolumes not recreated", dev.display())));
            continue;
        }
        report(Event::Step(format!("{}: waiting for btrfs to drop the subvolumes left out", dev.display())));
        match crate::fs::finish_exclusion(dev, &left.recreate, &left.deleted) {
            Ok(()) if left.recreate.is_empty() => {}
            Ok(()) => report(Event::Step(format!(
                "Recreated empty on {}: {}",
                dev.display(),
                left.recreate.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            ))),
            Err(e) => report(Event::Warning(format!("{}: {e}", dev.display()))),
        }
    }
    if plan.efi_fallback {
        for esp in work.esps {
            report(Event::Step(format!("Checking the EFI boot path on {}", esp.display())));
            match crate::fs::efi_fallback(&esp) {
                Ok(Some(msg)) => report(Event::Step(msg)),
                Ok(None) => {}
                Err(e) => report(Event::Warning(format!("{}: {e}", esp.display()))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::{local, version};

    const GIB: u64 = 1 << 30;

    fn plan(mode: Mode, disks: Vec<Option<usize>>) -> Plan {
        Plan { mode, disks, volumes: vec![], grow: false, efi_fallback: false, exclude_subvolumes: vec!["@x".into()] }
    }

    #[test]
    fn device_mode_writes_the_start_last() {
        let v = version();
        let locals = [local("sdx", 30 * GIB)];
        let Layout { mut jobs, targets, tables } = layout(&v, &plan(Mode::Device, vec![Some(0), None]), &locals).unwrap();
        assert!(tables.is_empty());
        // The first MiB held in memory, then the rest of the image.
        assert_eq!((jobs[0].range.offset, jobs[0].range.length, jobs[0].dev), (0, MIB, Some(0)));
        assert_eq!(jobs[0].table.as_ref().map(Vec::len), Some(MIB as usize));
        assert_eq!((jobs[1].range.offset, jobs[1].range.offset + jobs[1].range.length), (MIB, 20 * GIB));
        assert!(jobs[1].table.is_none());
        // Both tables cleared before, and again if the restore fails.
        let t = &targets[0];
        assert!(t.zero.contains(&(0, MIB)) && t.zero.contains(&(30 * GIB - 33 * 512, 33 * 512)));
        // Also where the backup table of the smaller image would be.
        assert_eq!(t.undo, vec![(0, 34 * 512), (30 * GIB - 33 * 512, 33 * 512), (20 * GIB - 33 * 512, 33 * 512)]);
        // Carving btrfs partitions out leaves the held start alone.
        let mut v2 = version();
        v2.disks[0].partitions[1].fs = "btrfs".into();
        let specials = carve(&mut jobs, &v2);
        assert_eq!(specials.len(), 1);
        assert!(jobs.iter().any(|j| j.range.offset == 0 && j.table.is_some()));
        let (shares, total) = deal(jobs);
        assert_eq!(total, 20 * GIB - v.disks[0].partitions[1].length);
        assert!(shares.iter().flatten().any(|j| j.table.is_some()));
        // A smaller disk: up to the end of the last partition, rounded to a MiB, never past the
        // end of the disk.
        let mut v = version();
        v.disks[0].partitions[1].length += 512;
        let end = used_end(&v.disks[0]);
        let small = [local("sdx", end + 33 * 512)];
        assert!(crate::plan::disk_fits(&v.disks[0], &small[0]).is_ok());
        let Layout { jobs, .. } = layout(&v, &plan(Mode::Device, vec![Some(0), None]), &small).unwrap();
        assert_eq!(jobs[1].range.offset + jobs[1].range.length, end + 33 * 512);
    }

    #[test]
    fn system_mode_on_a_partitioned_disk_keeps_its_table() {
        let v = version();
        let mut l = local("sdx", 30 * GIB);
        l.partitions = vec![crate::local::LocalPartition { number: 1, name: "sdx1".into(), offset: MIB, length: GIB, busy: None }];
        let Layout { jobs, targets, .. } = layout(&v, &plan(Mode::System, vec![Some(0), None]), &[l]).unwrap();
        assert!(jobs.iter().all(|j| j.table.is_none() && j.range.partition_number != 0));
        assert!(targets[0].zero.is_empty());
        // Undone: the signatures of the volumes written, not the table.
        assert!(targets[0].undo.contains(&(MIB, MIB)) && !targets[0].undo.contains(&(0, 34 * 512)));
    }

    #[test]
    fn volumes_need_a_target() {
        let v = version();
        let mut p = plan(Mode::Volumes, vec![None, None]);
        p.volumes = vec![crate::plan::VolumeTarget { disk: 1, part: 0, local: 0, place: None }];
        assert!(layout(&v, &p, &[local("sdx", 100 * GIB)]).is_err());
    }

    #[test]
    fn disk_identity() {
        let mut l = local("sdx", 30 * GIB);
        (l.rdev, l.id) = (0x810, "WD-123".into());
        assert!(same_disk(&l, 0x810, Some(30 * GIB), Some(512), "WD-123").is_ok());
        for (rdev, size, sector, id, what) in [
            (0x820, Some(30 * GIB), Some(512), "WD-123", "device number"),
            (0x810, Some(31 * GIB), Some(512), "WD-123", "size"),
            (0x810, None, Some(512), "WD-123", "size"),
            (0x810, Some(30 * GIB), Some(4096), "WD-123", "sector size"),
            (0x810, Some(30 * GIB), Some(512), "WD-456", "serial"),
        ] {
            assert!(same_disk(&l, rdev, size, sector, id).unwrap_err().contains(what), "{what}");
        }
    }

    #[test]
    fn undo_zeroes_on_failure_only() {
        let path = std::env::temp_dir().join(format!("abb-restore-undo-{}", std::process::id()));
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        f.write_all_at(&[1u8; 8192], 0).unwrap();
        let read = |o| {
            let mut b = [0u8; 1];
            f.read_exact_at(&mut b, o).unwrap();
            b[0]
        };
        let mut undo = Undo::default();
        undo.0.push((path.clone(), f.try_clone().unwrap(), vec![(0, 4096)]));
        undo.finish(false);
        assert_eq!(read(0), 1);
        undo.0.push((path.clone(), f.try_clone().unwrap(), vec![(0, 4096)]));
        let e = undo.fail("network down".into());
        assert!(e.starts_with("network down\n") && e.contains("incomplete"), "{e}");
        assert_eq!((read(0), read(4095), read(4096)), (0, 0, 1));
        // Nothing written yet: the error alone.
        assert_eq!(Undo::default().fail("refused".into()), "refused");
        // Dropped without `finish` (a panic): zeroed too.
        f.write_all_at(&[1u8; 10], 0).unwrap();
        drop(Undo(vec![(path.clone(), f.try_clone().unwrap(), vec![(0, 10)])]));
        assert_eq!(read(9), 0);
        std::fs::remove_file(path).unwrap();
    }
}
