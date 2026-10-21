//! Running a restore: download the image ranges of the plan, write them to the local disks,
//! then fix up partition tables, filesystem sizes and the EFI boot path.

use crate::gpt::{self, Gpt};
use crate::local::LocalDisk;
use crate::plan::{Mode, Place, Plan, used_end};
use crate::server::{Chunk, Range, Server};
use crate::spec::{Partition, Task, Version};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
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

fn discard(f: &File, offset: u64, length: u64) {
    let range = [offset, length];
    unsafe { libc::ioctl(f.as_raw_fd(), BLKDISCARD as _, range.as_ptr()) };
}

/// Clears the places where filesystems keep their signatures, so that what was on the disk before
/// cannot be mistaken for the restored filesystem: the first MiB and the btrfs superblock copies.
fn wipe_signatures(f: &File, offset: u64, length: u64) -> std::io::Result<()> {
    let zero = vec![0u8; MIB as usize];
    f.write_all_at(&zero[..MIB.min(length) as usize], offset)?;
    for mirror in [64 * MIB, 256 << 30] {
        if mirror + 4096 <= length {
            f.write_all_at(&zero[..4096], offset + mirror)?;
        }
    }
    Ok(())
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

pub fn run(
    server: &mut Server,
    task: &Task,
    v: &Version,
    plan: &Plan,
    locals: &[LocalDisk],
    cancel: &AtomicBool,
    report: &mut dyn FnMut(Event),
) -> Result<(), String> {
    report(Event::Step("Preparing the disks".into()));
    let (mut jobs, devices) = prepare(v, plan, locals)?;
    let specials = if plan.exclude_subvolumes.is_empty() { vec![] } else { carve(&mut jobs, v) };
    let mode = plan.mode.code();
    let start = |s: &mut Server, ranges: &[Range]| s.start(task.device_id, task.task_id, v.id, mode, &crate::local::machine_uuid(), ranges);
    report(Event::Step("Restoring".into()));
    let result = transfer(server, &start, std::mem::take(&mut jobs), &devices, cancel, report).map(|j| jobs = j);
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
        for d in &devices {
            d.file.sync_all().map_err(|e| format!("{}: {e}", d.path.display()))?;
        }
        report(Event::Step("Fixing up partition tables".into()));
        finish_disks(v, plan, locals, &jobs, &devices, report)
    });
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
        let Some(dev) = job.dev else {
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

/// Opens the target devices, clears old signatures and lists the ranges to download.
fn prepare(v: &Version, plan: &Plan, locals: &[LocalDisk]) -> Result<(Vec<Job>, Vec<Device>), String> {
    let mut jobs = Vec::new();
    let mut devices: Vec<Device> = Vec::new();
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
    match plan.mode {
        Mode::Device | Mode::System => {
            for (i, m) in plan.disks.iter().enumerate() {
                let Some(j) = *m else { continue };
                let (d, l) = (&v.disks[i], &locals[j]);
                let file = open_excl(&l.path())?;
                let dev = devices.len();
                if plan.mode == Mode::Device {
                    let length = if l.size >= d.size { d.size } else { used_end(d).div_ceil(MIB) * MIB };
                    discard(&file, 0, l.size);
                    for p in &d.partitions {
                        wipe_signatures(&file, p.offset, p.length).map_err(|e| format!("{}: {e}", l.name))?;
                    }
                    jobs.push(Job { range: range(d, None, 0, length), dev: Some(dev), delta: 0, table: None, mirrors: vec![] });
                } else {
                    let first = d.partitions.first().map_or(MIB, |p| p.offset);
                    if l.partitions.is_empty() {
                        jobs.push(Job { range: range(d, None, 0, first), dev: Some(dev), delta: 0, table: None, mirrors: vec![] });
                    }
                    for p in d.partitions.iter().filter(|p| p.is_system() && !p.excluded) {
                        discard(&file, p.offset, p.length);
                        wipe_signatures(&file, p.offset, p.length).map_err(|e| format!("{}: {e}", l.name))?;
                        jobs.push(Job { range: range(d, Some(p), p.offset, p.length), dev: Some(dev), delta: 0, table: None, mirrors: vec![] });
                    }
                }
                devices.push(Device { path: l.path(), file, disk: Some(j) });
            }
        }
        Mode::Volumes => {
            // New partitions first, so that each volume is written where it will stay.
            let mut tables: Vec<(usize, Gpt)> = Vec::new();
            for t in &plan.volumes {
                let Place::Free(start) = t.place else { continue };
                let (p, l) = (&v.disks[t.disk].partitions[t.part], &locals[t.local]);
                if !tables.iter().any(|(k, _)| *k == t.local) {
                    tables.push((t.local, l.gpt.clone().unwrap_or_else(|| Gpt::new(l.sector_size))));
                }
                let g = &mut tables.iter_mut().find(|(k, _)| *k == t.local).unwrap().1;
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
            }
            for (k, g) in &tables {
                let l = &locals[*k];
                let file = open_excl(&l.path())?;
                g.write(&file, l.size, l.gpt.is_none()).map_err(|e| format!("{}: {e}", l.name))?;
                reread_partitions(&file)?;
            }
            let mut sources: Vec<usize> = Vec::new();
            for t in &plan.volumes {
                let (d, p, l) = (&v.disks[t.disk], &v.disks[t.disk].partitions[t.part], &locals[t.local]);
                let number = match t.place {
                    Place::Partition(n) => n,
                    Place::Free(start) => {
                        let g = &tables.iter().find(|(k, _)| *k == t.local).unwrap().1;
                        g.used().find(|(_, e)| e.first_lba == start).map(|(n, _)| n).unwrap()
                    }
                };
                let path = l.partition_path(number);
                if !wait_for(&path) {
                    return Err(format!("{} did not appear", path.display()));
                }
                let file = open_excl(&path)?;
                let size = file.metadata().ok().and_then(|_| device_size(&file)).unwrap_or(p.length);
                discard(&file, 0, size);
                wipe_signatures(&file, 0, size).map_err(|e| format!("{}: {e}", path.display()))?;
                let dev = devices.len();
                devices.push(Device { path, file, disk: None });
                jobs.push(Job { range: range(d, Some(p), p.offset, p.length), dev: Some(dev), delta: -(p.offset as i64), table: None, mirrors: vec![] });
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
    Ok((jobs, devices))
}

fn device_size(f: &File) -> Option<u64> {
    const BLKGETSIZE64: u32 = 0x80081272;
    let mut size: u64 = 0;
    (unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64 as _, &mut size) } == 0).then_some(size)
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
    let shares: Vec<std::sync::Mutex<Vec<Job>>> = shares.into_iter().map(std::sync::Mutex::new).collect();
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
            let (shares, next, done, failed, errors, warnings, running_streams) = (&shares, &next, &done, &failed, &errors, &warnings, &running_streams);
            sc.spawn(move || {
                while !failed.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
                    let Some(share) = shares.get(next.fetch_add(1, Ordering::Relaxed)) else { break };
                    let mut share = share.lock().unwrap();
                    let ranges: Vec<Range> = share.iter().map(|j| j.range.clone()).collect();
                    let res = start(&mut s, &ranges).map_err(|e| format!("the server refused the restore: {e}")).and_then(|running| {
                        let res = download(&mut s, running, &mut share, devices, done, cancel, &|w| warnings.lock().unwrap().push(w));
                        let (status, code) = match &res {
                            Ok(()) => ("complete", LOG_RESTORE_FINISH),
                            Err(_) if cancel.load(Ordering::Relaxed) => ("cancel", LOG_RESTORE_CANCEL),
                            Err(_) => ("error", LOG_INTERNAL_ERROR),
                        };
                        let _ = s.finish(running, status, code);
                        res
                    });
                    if let Err(e) = res {
                        failed.store(true, Ordering::Relaxed);
                        errors.lock().unwrap().push(e);
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
    if let Some(e) = errors.into_inner().unwrap().into_iter().next() {
        return Err(e);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    Ok(shares.into_iter().flat_map(|s| s.into_inner().unwrap()).collect())
}

/// Downloads the ranges of one restore task into their jobs; `done` counts the bytes written.
pub(crate) fn download(
    server: &mut Server,
    running: u64,
    jobs: &mut [Job],
    devices: &[Device],
    done: &AtomicU64,
    cancel: &AtomicBool,
    warn: &dyn Fn(String),
) -> Result<(), String> {
    // Jobs in order of image and offset, to find the one a chunk belongs to by bisection.
    jobs.sort_by(|a, b| (&a.range.image_path, a.range.offset).cmp(&(&b.range.image_path, b.range.offset)));
    let find = |jobs: &[Job], path: &str, offset: u64| {
        let i = jobs.partition_point(|j| (j.range.image_path.as_str(), j.range.offset) <= (path, offset));
        i.checked_sub(1).filter(|&i| jobs[i].range.image_path == path)
    };
    let mut attempt = 0;
    loop {
        let ranges: Vec<Range> = jobs.iter().map(|j| j.range.clone()).collect();
        let res = server.download(running, &ranges, |c| {
            if cancel.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("cancelled"));
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
                            devices[d].file.write_all_at(data, (offset as i64 + delta) as u64)?;
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
        match res {
            Ok(()) => {
                if let Some(j) = jobs.iter().find(|j| j.range.done < j.range.length) {
                    return Err(format!("the server ended the transfer before the end of {}", j.range.image_path));
                }
                return Ok(());
            }
            Err(e) if cancel.load(Ordering::Relaxed) => return Err(format!("cancelled ({e})")),
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
                    .map_err(|e| format!("{}: {e}", l.name))?
                    .ok_or_else(|| format!("{}: the restored partition table is not valid", l.name))?;
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
                g.write(&dev.file, l.size, false).map_err(|e| format!("{}: {e}", l.name))?;
                unsafe { libc::ioctl(dev.file.as_raw_fd(), BLKFLSBUF as _) };
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
                    Place::Free(start) => start,
                    Place::Partition(n) => {
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
                    .find(|j| j.table.is_some() && j.range.image_path == d.image_path)
                    .and_then(|j| Gpt::parse(j.table.as_ref().unwrap(), d.sector_size))
                else {
                    continue;
                };
                let Some((_, orig)) = src.used().find(|(_, e)| e.first_lba * d.sector_size == p.offset) else { continue };
                let file = File::options().read(true).write(true).open(l.path()).map_err(|e| format!("{}: {e}", l.name))?;
                let mut g = Gpt::read(&file, l.sector_size).map_err(|e| e.to_string())?.ok_or("partition table lost")?;
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
                g.write(&file, l.size, false).map_err(|e| format!("{}: {e}", l.name))?;
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
