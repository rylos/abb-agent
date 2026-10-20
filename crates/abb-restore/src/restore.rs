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
use std::sync::atomic::{AtomicBool, Ordering};
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
struct Job {
    range: Range,
    /// Index in `devices`; None for data kept in memory (`table`).
    dev: Option<usize>,
    /// Device offset = image offset + delta.
    delta: i64,
    /// Original partition table, read to copy partition types and names.
    table: Option<Vec<u8>>,
}

struct Device {
    path: PathBuf,
    file: File,
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
    let ranges: Vec<Range> = jobs.iter().map(|j| j.range.clone()).collect();
    let total: u64 = ranges.iter().map(|r| r.length).sum();

    report(Event::Step("Starting the restore on the server".into()));
    let running = server
        .start(task.device_id, task.task_id, v.id, plan.mode.code(), &crate::local::machine_uuid(), &ranges)
        .map_err(|e| format!("the server refused the restore: {e}"))?;

    report(Event::Step("Restoring".into()));
    let result = download(server, running, &mut jobs, &devices, total, cancel, report);
    let result = result.and_then(|_| {
        for d in &devices {
            d.file.sync_all().map_err(|e| format!("{}: {e}", d.path.display()))?;
        }
        report(Event::Step("Fixing up partition tables".into()));
        finish_disks(v, plan, locals, &jobs, &devices, report)
    });
    // Mounting and checking need the devices that are open exclusively here.
    drop(devices);
    let result = result.map(|(grown, esps)| after_restore(plan, grown, esps, report));
    let (status, code) = match &result {
        Ok(()) => ("complete", LOG_RESTORE_FINISH),
        Err(_) if cancel.load(Ordering::Relaxed) => ("cancel", LOG_RESTORE_CANCEL),
        Err(_) => ("error", LOG_INTERNAL_ERROR),
    };
    if let Err(e) = server.finish(running, status, code) {
        report(Event::Warning(format!("could not report the result to the server: {e}")));
    }
    result
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
                    jobs.push(Job { range: range(d, None, 0, length), dev: Some(dev), delta: 0, table: None });
                } else {
                    let first = d.partitions.first().map_or(MIB, |p| p.offset);
                    if l.partitions.is_empty() {
                        jobs.push(Job { range: range(d, None, 0, first), dev: Some(dev), delta: 0, table: None });
                    }
                    for p in d.partitions.iter().filter(|p| p.is_system() && !p.excluded) {
                        discard(&file, p.offset, p.length);
                        wipe_signatures(&file, p.offset, p.length).map_err(|e| format!("{}: {e}", l.name))?;
                        jobs.push(Job { range: range(d, Some(p), p.offset, p.length), dev: Some(dev), delta: 0, table: None });
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
                jobs.push(Job { range: range(d, Some(p), p.offset, p.length), dev: Some(dev), delta: -(p.offset as i64), table: None });
                if !sources.contains(&t.disk) {
                    sources.push(t.disk);
                }
            }
            // The original partition tables, to give new partitions their type and name.
            for i in sources {
                let d = &v.disks[i];
                if d.gpt {
                    let len = d.partitions.first().map_or(MIB, |p| p.offset).min(MIB);
                    jobs.push(Job { range: range(d, None, 0, len), dev: None, delta: 0, table: Some(vec![0; len as usize]) });
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

fn download(
    server: &mut Server,
    running: u64,
    jobs: &mut [Job],
    devices: &[Device],
    total: u64,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(Event),
) -> Result<(), String> {
    let mut attempt = 0;
    loop {
        let ranges: Vec<Range> = jobs.iter().map(|j| j.range.clone()).collect();
        let res = server.download(running, &ranges, |c| {
            if cancel.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("cancelled"));
            }
            match c {
                Chunk::Data { image_path, offset, data } => {
                    let job = jobs
                        .iter_mut()
                        .find(|j| j.range.image_path == image_path && offset >= j.range.offset && offset < j.range.offset + j.range.length)
                        .ok_or_else(|| std::io::Error::other(format!("{image_path} offset {offset} outside the requested ranges")))?;
                    let end = (offset + data.len() as u64).min(job.range.offset + job.range.length);
                    let data = &data[..(end - offset) as usize];
                    if let Some(t) = &mut job.table {
                        let s = (offset - job.range.offset) as usize;
                        t[s..s + data.len()].copy_from_slice(data);
                    } else if let Some(d) = job.dev {
                        devices[d].file.write_all_at(data, (offset as i64 + job.delta) as u64)?;
                    }
                    job.range.done = end - job.range.offset;
                }
                Chunk::EndOfRange { image_path, offset } => {
                    if let Some(j) = jobs.iter_mut().find(|j| j.range.image_path == image_path && j.range.offset + j.range.length == offset) {
                        j.range.done = j.range.length;
                    }
                }
                Chunk::Done => {}
            }
            report(Event::Progress { done: jobs.iter().map(|j| j.range.done).sum(), total });
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
                report(Event::Warning(format!("connection lost ({e}), resuming (attempt {attempt})")));
                std::thread::sleep(Duration::from_secs((1u64 << attempt).min(30)));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Filesystems to grow (device, type) and EFI system partitions to check.
type AfterWork = (Vec<(PathBuf, String)>, Vec<PathBuf>);

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
    let mut grown: Vec<(PathBuf, String)> = Vec::new();
    let mut esps: Vec<PathBuf> = Vec::new();
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
                    esps.extend(d.partitions.iter().filter(|p| p.is_esp()).filter_map(|p| {
                        l.partitions.iter().find(|q| q.offset == p.offset).map(|q| l.partition_path(q.number))
                    }));
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
                            && (p.fs.starts_with("ext") || p.fs == "btrfs")
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
                        if plan.grow {
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
                    if plan.grow {
                        grown.push((l.partition_path(n as u64 + 1), p.fs.clone()));
                    }
                }
                g.write(&file, l.size, false).map_err(|e| format!("{}: {e}", l.name))?;
                if let Err(e) = reread_partitions(&file) {
                    report(Event::Warning(e));
                }
            }
        }
    }
    Ok((grown, esps))
}

/// Growing filesystems and the EFI boot path, once the disks are closed.
fn after_restore(plan: &Plan, grown: Vec<(PathBuf, String)>, esps: Vec<PathBuf>, report: &mut dyn FnMut(Event)) {
    for (path, fs) in grown {
        report(Event::Step(format!("Growing {fs} on {}", path.display())));
        if !wait_for(&path) {
            report(Event::Warning(format!("{} did not appear: filesystem not grown", path.display())));
            continue;
        }
        if let Err(e) = crate::fs::grow(&path, &fs) {
            report(Event::Warning(format!("{}: filesystem not grown: {e}", path.display())));
        }
    }
    if plan.efi_fallback {
        for esp in esps {
            report(Event::Step(format!("Checking the EFI boot path on {}", esp.display())));
            match crate::fs::efi_fallback(&esp) {
                Ok(Some(msg)) => report(Event::Step(msg)),
                Ok(None) => {}
                Err(e) => report(Event::Warning(format!("{}: {e}", esp.display()))),
            }
        }
    }
}
