//! Running a backup: disks and volumes, device spec, start and completion on the server;
//! the block stream is in `stream.rs`.

use crate::disk::{self, Disk, Fs, Partition};
use crate::host::now;
use crate::session::{RunningTask, Session, get_int};
use crate::control;
use crate::scripts::{self, Hook};
use crate::state::LastRun;
use crate::task::TaskInfo;
use crate::stream::{self, MIB, Piece};
use crate::volume::{self, Vol};
use abb_proto::transport::{Error, Result};
use abb_proto::{Connection, Value};
use btrfs_image::cbt::Digests;
use serde_json::{Value as Json, json};
use std::path::Path;
use std::time::Instant;

const GIB: f64 = (1u64 << 30) as f64;

/// `log_report` codes.
const LOG_DEVICE_INFO: u64 = 7001;
const LOG_SNAPSHOT_START: u64 = 7002;
const LOG_SNAPSHOT_DONE: u64 = 7003;
const LOG_UPLOAD_SPEC_START: u64 = 7009;
const LOG_UPLOAD_SPEC_DONE: u64 = 7010;
pub const LOG_VOLUME_START: u64 = 7012;
pub const LOG_VOLUME_DONE: u64 = 7013;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Copied raw from the disk (GPT tables, ESP, BIOS boot).
    Raw,
    /// Volume with a consistent image (btrfs, ext): index into the volume list.
    Vol(usize),
    /// Not sent (interval type 5).
    Excluded(Skip),
}

/// Why a partition is left out of the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Filesystem not supported, or not readable consistently (backup is partial).
    Unsupported,
    /// Not selected by the task's source type.
    NotSelected,
    Swap,
}

/// What the task asks to back up (the ABB "source type").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// 1: every internal disk, whole.
    WholeDevice,
    /// 2: the system volumes (`/`, `/boot`, EFI and BIOS boot partitions) and the disks holding them.
    System,
    /// 3: the volumes mounted at these paths, plus the system volumes if `/` is among them.
    Custom(Vec<String>),
}

impl std::fmt::Display for Selection {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Selection::WholeDevice => f.write_str("entire device"),
            Selection::System => f.write_str("system volumes"),
            Selection::Custom(paths) => write!(f, "custom volumes: {}", paths.join(", ")),
        }
    }
}

impl Selection {
    /// From a `get_task` entry: `source_type` and `custom_volume` (a JSON array of mount paths,
    /// sent as a string).
    pub fn from_task(task: &Value) -> Selection {
        let paths = |v: Option<&Value>| -> Vec<String> {
            match v {
                Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(normalize_mount)).collect(),
                Some(v) => v
                    .as_str()
                    .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                    .unwrap_or_default()
                    .iter()
                    .map(|x| normalize_mount(x))
                    .collect(),
                None => Vec::new(),
            }
        };
        match task.get("source_type").and_then(Value::as_int) {
            Some(2) => Selection::System,
            Some(3) => Selection::Custom(paths(task.get("custom_volume"))),
            _ => Selection::WholeDevice,
        }
    }

    fn selects(&self, p: &Partition) -> bool {
        match self {
            Selection::WholeDevice => true,
            Selection::System => is_system(p),
            Selection::Custom(list) => {
                p.mounts.iter().any(|m| list.contains(m)) || (list.iter().any(|m| m == "/") && is_system(p))
            }
        }
    }
}

fn normalize_mount(m: &str) -> String {
    let t = m.trim_end_matches('/');
    if t.is_empty() { "/".into() } else { t.into() }
}

/// System volume: holds `/` or `/boot` (also `/boot/efi`, `/efi`), or is an EFI or BIOS boot
/// partition.
fn is_system(p: &Partition) -> bool {
    p.is_esp()
        || p.is_bios_boot()
        || p.mounts.iter().any(|m| m == "/" || m == "/boot" || m.starts_with("/boot/") || m == "/efi")
}

#[derive(Clone, Debug)]
pub struct Interval {
    pub start: u64,
    pub end: u64,
    /// 1 volume with snapshot, 2 raw partition, 3 area outside the partitions, 5 skipped.
    pub kind: u8,
    pub part: Option<Partition>,
    pub source: Source,
}

/// Disk intervals and volumes to prepare. A read-write mounted ext is backed up only with
/// `freeze_ext` (frozen for the whole upload), otherwise it is excluded (partial backup).
fn plan(d: &Disk, volumes: &mut Vec<Partition>, freeze_ext: bool, sel: &Selection) -> Vec<Interval> {
    let mut out = Vec::new();
    let mut pos = 0;
    let gap = |s: u64, e: u64| Interval { start: s, end: e, kind: 3, part: None, source: Source::Raw };
    for p in &d.partitions {
        if p.offset > pos {
            out.push(gap(pos, p.offset));
        }
        let vol = matches!(p.fs, Fs::Btrfs) || (matches!(p.fs, Fs::Ext(_)) && volume::ext_readable(p, freeze_ext));
        let (kind, source) = if p.fs == Fs::Swap {
            (5, Source::Excluded(Skip::Swap))
        } else if !sel.selects(p) {
            (5, Source::Excluded(Skip::NotSelected))
        } else if vol {
            volumes.push(p.clone());
            (1, Source::Vol(volumes.len() - 1))
        } else if matches!(p.fs, Fs::None | Fs::Vfat) {
            (2, Source::Raw)
        } else {
            (5, Source::Excluded(Skip::Unsupported))
        };
        out.push(Interval { start: p.offset, end: p.offset + p.length, kind, part: Some(p.clone()), source });
        pos = p.offset + p.length;
    }
    if pos < d.size {
        out.push(gap(pos, d.size));
    }
    out
}

fn skip_of(intervals: &[Interval], p: &Partition) -> Option<Skip> {
    intervals.iter().find(|i| i.part.as_ref().is_some_and(|q| q.name == p.name)).and_then(|i| match i.source {
        Source::Excluded(why) => Some(why),
        _ => None,
    })
}

/// Unsupported volumes and swap are reported as excluded, as the official agent does; volumes
/// left out by the source type are simply not transferred.
fn is_excluded(intervals: &[Interval], p: &Partition) -> bool {
    matches!(skip_of(intervals, p), Some(Skip::Unsupported | Skip::Swap))
}

/// The image can boot: no system volume is missing (swap does not count).
fn is_bootable(disks: &[(Disk, Vec<Interval>)]) -> bool {
    disks.iter().any(|(d, _)| d.partitions.iter().any(|p| p.mounts.iter().any(|m| m == "/")))
        && disks.iter().all(|(d, iv)| {
            d.partitions
                .iter()
                .filter(|p| is_system(p))
                .all(|p| matches!(skip_of(iv, p), None | Some(Skip::Swap)))
        })
}

/// Excluded volumes that contain data ("partial" backup); swap does not count.
fn unsupported<'a>(d: &'a Disk, intervals: &[Interval]) -> Vec<&'a Partition> {
    d.partitions.iter().filter(|p| skip_of(intervals, p) == Some(Skip::Unsupported)).collect()
}

pub struct Job<'a> {
    pub sess: &'a Session,
    pub rt: RunningTask,
    result_id: u64,
    run_name: String,
}

impl Job<'_> {
    pub fn log(&self, c: &mut Connection, log_type: u64, volume: Option<&str>) {
        let mut params = json!({
            "result_id": self.result_id,
            "running_task_id": self.rt.running_task_id,
            "task_id": self.rt.task_id,
            "task_uuid": self.rt.task_uuid,
        });
        if let Some(v) = volume {
            params["volume_name"] = json!(v);
        }
        let entry = Value::map([
            ("log_time", Value::Int(now())),
            ("log_type", Value::Int(log_type)),
            ("other_params", Value::str(serde_json::to_string_pretty(&params).unwrap() + "\n")),
            ("running_task_id", Value::Int(self.rt.running_task_id)),
        ]);
        let r = self.sess.call(
            c,
            "log_report",
            vec![("subaction", Value::str("result_detail")), ("logs", Value::Array(vec![entry]))],
        );
        if let Err(e) = r {
            eprintln!("log_report {log_type}: {e}");
        }
    }
}

pub fn alias(p: &Partition) -> String {
    if p.mount_path.is_empty() { format!("/dev/{}", p.name) } else { p.mount_path.clone() }
}

/// `used`: bytes of each disk in use; `transfer`: bytes that this backup reads and sends (all
/// of the used ones for a full backup, the changed ones for an incremental), the total of the
/// console's progress bar.
fn spec(sess: &Session, disks: &[(Disk, Vec<Interval>)], used: &[u64], transfer: &[u64], incremental: bool, start_time: u64, run_name: &str) -> Json {
    let mut disk_layout = serde_json::Map::new();
    let mut mount_map = serde_json::Map::new();
    let mut images = Vec::new();
    let mut disk_images = Vec::new();
    let mut unsupported_volumes = Vec::new();
    for (n, (d, intervals)) in disks.iter().enumerate() {
        let mut parts = Vec::new();
        for p in &d.partitions {
            let excluded = is_excluded(intervals, p);
            parts.push(json!({
                "alias_name": alias(p),
                "attribute": p.attribute(),
                "bytes_per_sector": d.sector_size,
                "device_name": format!("/dev/{}", p.name),
                "disk_number": n,
                "dummy_disk_length": "0",
                "exclude_reason": if excluded { 6 } else { 0 },
                "file_system": p.fs.name(),
                "is_excluded": excluded,
                "is_primary_partition": false,
                "length": p.length.to_string(),
                "mount_path": p.mount_path,
                "offset": p.offset.to_string(),
                "partition_number": p.number,
                "volume_name": p.fs_uuid,
            }));
            // The server mounts every volume of the mount map to verify the new version: only
            // the volumes actually sent may be listed, or the version is marked as failed.
            if !p.mount_path.is_empty() && skip_of(intervals, p).is_none() {
                mount_map.insert(
                    p.mount_path.clone(),
                    json!({
                        "alias_name": alias(p),
                        "device_name": format!("/dev/{}", p.name),
                        "disk_number": n,
                        "fs_type": "",
                        "guest_mount_point": "",
                        "image_path": d.image_name(),
                        "is_mbr": false,
                        "is_primary_partition": false,
                        "is_system": is_system(p),
                        "length": p.length.to_string(),
                        "mount_path": p.mount_path,
                        "object_id": 0,
                        "offset": p.offset.to_string(),
                        "partition_number": p.number,
                        "target_partition_number": -1,
                        "valid_offset_length": true,
                        "volume_uuid": "",
                    }),
                );
            }
        }
        for p in unsupported(d, intervals) {
            unsupported_volumes.push(json!({ "alias": alias(p), "uuid": p.fs_uuid }));
        }
        disk_layout.insert(n.to_string(), Json::Array(parts));

        let excluded_length: u64 =
            intervals.iter().filter(|i| i.kind == 2 || i.kind == 3).map(|i| i.end - i.start).sum();
        images.push(json!({
            "changed_length": if incremental { transfer[n] } else { d.size }.to_string(),
            "excluded_length": excluded_length.to_string(),
            "image_name": d.image_name(),
            "length": d.size.to_string(),
            "original_used_length": used[n].to_string(),
            "transfer_length": transfer[n].to_string(),
            "transfer_length_4k": transfer[n].to_string(),
            "used_length": d.size.to_string(),
        }));
        let ivs: Vec<Json> = intervals
            .iter()
            .map(|i| {
                let p = i.part.as_ref();
                let (path, snapshot_id, src_start) = match i.source {
                    Source::Vol(_) => (format!("/dev/{}", p.unwrap().name), run_name.to_string(), 0),
                    _ => (format!("/dev/{}", d.name), String::new(), i.start),
                };
                json!({
                    "alias_name": p.map(alias).unwrap_or_default(),
                    "destination_end": i.end.to_string(),
                    "destination_start": i.start.to_string(),
                    "device_name": p.map(|p| format!("/dev/{}", p.name)).unwrap_or_default(),
                    "disk_end": i.end.to_string(),
                    "disk_start": i.start.to_string(),
                    "file_system": p.map(|p| p.fs.name().to_string()).unwrap_or_default(),
                    "is_primary_partition": false,
                    "is_system": p.is_some(),
                    "is_system_drive": p.is_some_and(|p| p.mount_path == "/"),
                    "mount_path": p.map(|p| p.mount_path.clone()).unwrap_or_default(),
                    "path": path,
                    "snapshot_id": snapshot_id,
                    "source_end": (src_start + i.end - i.start).to_string(),
                    "source_start": src_start.to_string(),
                    "type": i.kind,
                    "volume_name": p.map(|p| p.fs_uuid.clone()).unwrap_or_default(),
                })
            })
            .collect();
        disk_images.push(json!({
            "disk_layout": d.layout,
            "disk_number": n.to_string(),
            "image_name": d.image_name(),
            "intervals": ivs,
            "need_upload": true,
        }));
    }
    json!({
        "agent_version": abb_proto::transport::AGENT_BUILD,
        "device_spec": {
            "cpu_number": sess.host.cpu_number,
            "disk_layout": disk_layout,
            "images": images,
            "is_bootable": is_bootable(disks),
            "is_system_mbr": true,
            "is_volume": false,
            "memory_size": sess.host.memory_size,
            "mount_map": mount_map,
            "snapshot_start_time": start_time.to_string(),
            "unsupported_volumes": unsupported_volumes,
            "volume_groups": [],
        },
        "disk_images": disk_images,
    })
}

/// Used bytes (whole MiB) of a disk according to the plan.
fn used_estimate(intervals: &[Interval], volumes: &[Vol], d: &Disk) -> u64 {
    let mut total = 0;
    let mut w = 0;
    while w < d.size {
        let we = (w + MIB).min(d.size);
        let used = intervals.iter().any(|i| {
            i.start < we
                && i.end > w
                && match i.source {
                    Source::Raw => true,
                    Source::Excluded(_) => false,
                    Source::Vol(v) => {
                        let off = i.part.as_ref().unwrap().offset;
                        volumes[v].any_used(w.max(i.start) - off, we.min(i.end) - off)
                    }
                }
        });
        if used {
            total += we - w;
        }
        w = we;
    }
    total
}

fn cbt_path(p: &Partition) -> std::path::PathBuf {
    Path::new(crate::state::STATE_DIR).join("cbt").join(format!("{}.cbt", p.fs_uuid))
}

/// On btrfs the CBT tables live in a subvolume of their own, left out of the images: they are
/// rewritten after every backup (a hundred MB on a 1 TB disk) and would otherwise be sent again
/// each time. Turns an existing directory into a subvolume. Returns its path if it is one.
pub fn cbt_subvolume() -> Option<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
    let state = Path::new(crate::state::STATE_DIR);
    let dir = state.join("cbt");
    std::fs::create_dir_all(state).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let c = std::ffi::CString::new(crate::state::STATE_DIR).ok()?;
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 || st.f_type as i64 != BTRFS_SUPER_MAGIC {
        return None;
    }
    match std::fs::metadata(&dir) {
        Ok(m) if m.ino() == 256 => return Some(dir),
        Ok(_) => {
            let new = state.join("cbt.new");
            if std::fs::metadata(&new).is_ok_and(|m| m.ino() == 256) {
                let _ = btrfs_image::snapshot::destroy(state, "cbt.new");
            }
            btrfs_image::snapshot::create_subvolume(state, "cbt.new").map_err(|e| eprintln!("CBT subvolume: {e}")).ok()?;
            // Files cannot be renamed across subvolumes: copied, then removed.
            let moved = (|| -> std::io::Result<()> {
                for e in std::fs::read_dir(&dir)?.flatten() {
                    std::fs::copy(e.path(), new.join(e.file_name()))?;
                }
                std::fs::remove_dir_all(&dir)?;
                std::fs::rename(&new, &dir)
            })();
            if let Err(e) = moved {
                eprintln!("CBT subvolume: {e}");
                let _ = btrfs_image::snapshot::destroy(state, "cbt.new");
                return None;
            }
        }
        Err(_) => btrfs_image::snapshot::create_subvolume(state, "cbt").map_err(|e| eprintln!("CBT subvolume: {e}")).ok()?,
    }
    Some(dir)
}

/// CBT tables of the previous backup, if they all match `base_version_id`.
fn previous_digests(volumes: &[Partition], base_version_id: u64) -> Option<Vec<Digests>> {
    if base_version_id == 0 || volumes.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for p in volumes {
        match Digests::load(&cbt_path(p)) {
            Ok((d, meta)) if meta.version_id == base_version_id && meta.partition_offset == p.offset => out.push(d),
            Ok((_, meta)) => {
                eprintln!("{}: CBT table is from version {} (base {base_version_id}): full backup", alias(p), meta.version_id);
                return None;
            }
            Err(e) => {
                eprintln!("{}: no CBT table ({e}): full backup", alias(p));
                return None;
            }
        }
    }
    Some(out)
}

/// Who started the backup.
pub enum Trigger {
    /// The server queued a running task (e.g. "Back up now" from the console).
    Server(RunningTask),
    /// The agent (local schedule or `backup` command): `client_trigger/add_backup_task`.
    Local { trigger_type: u64, trigger_event: Option<u64> },
}

/// `trigger_type` for `add_backup_task`: 2 for scheduled runs, as the official agent does,
/// 1 for manual runs.
pub const TRIGGER_SCHEDULE: u64 = 2;
pub const TRIGGER_MANUAL: u64 = 1;
/// Event trigger (`trigger_event` 3 = system startup).
pub const TRIGGER_EVENT: u64 = 3;
pub const EVENT_STARTUP: u64 = 3;

const LOCK: &str = "/run/lock/abb-agent.lock";
fn flock(path: &str, create: bool, wait: bool) -> Result<Option<std::fs::File>> {
    let f = match std::fs::OpenOptions::new().read(true).write(create).create(create).open(path) {
        Ok(f) => f,
        Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let op = libc::LOCK_EX | if wait { 0 } else { libc::LOCK_NB };
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), op) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(Error::Protocol("a backup is already running".into()));
        }
        return Err(e.into());
    }
    Ok(Some(f))
}

/// Disks with their intervals, and the partitions of the volumes to prepare.
type Scan = (Vec<(Disk, Vec<Interval>)>, Vec<Partition>);

/// Disks and volumes to back up for `sel`. Without `disks` in the configuration every internal
/// disk is a candidate; disks without a GPT table are skipped (with a warning, and the backup is
/// partial if they have partitions), and disks with nothing selected are not sent.
fn scan_disks(sel: &Selection) -> Result<(Scan, bool)> {
    let cfg = crate::config::get();
    let explicit = !cfg.disks.is_empty();
    let names = if explicit {
        let mut names = Vec::new();
        for e in &cfg.disks {
            let n = disk::resolve(e)?;
            // "system" and a path may name the same disk.
            if !names.contains(&n) {
                names.push(n);
            }
        }
        names
    } else {
        disk::internal_disks()?
    };
    let mut disks = Vec::new();
    let mut volume_parts = Vec::new();
    let mut missed = false;
    for n in names {
        let d = match disk::scan(&n) {
            Ok(d) => d,
            Err(e) if !explicit => {
                let has_parts = disk::has_partitions(&n);
                eprintln!("{n}: not backed up: {e}");
                missed |= has_parts;
                continue;
            }
            Err(e) => return Err(Error::Protocol(format!("{n}: {e}"))),
        };
        let mut parts = Vec::new();
        let iv = plan(&d, &mut parts, cfg.ext.freeze_mounted, sel);
        let wanted = *sel == Selection::WholeDevice
            || iv.iter().any(|i| i.part.is_some() && !matches!(i.source, Source::Excluded(_)));
        if !wanted {
            continue;
        }
        // Volume indices are global across disks.
        let base = volume_parts.len();
        let iv = iv
            .into_iter()
            .map(|mut i| {
                if let Source::Vol(v) = i.source {
                    i.source = Source::Vol(base + v);
                }
                i
            })
            .collect();
        volume_parts.extend(parts);
        disks.push((d, iv));
    }
    if let Selection::Custom(list) = sel {
        for m in list {
            if !disks.iter().any(|(d, _)| d.partitions.iter().any(|p| p.mounts.contains(m))) {
                eprintln!("selected volume {m}: not found");
                missed = true;
            }
        }
    }
    if disks.is_empty() {
        return Err(Error::Protocol("nothing to back up".into()));
    }
    let mut only: Vec<Disk> = disks.iter().map(|(d, _)| d.clone()).collect();
    crate::state::image_names(&mut only);
    for ((d, _), named) in disks.iter_mut().zip(only) {
        d.image = named.image;
    }
    Ok(((disks, volume_parts), missed))
}

fn remove_stale_snapshots(volume_parts: &[Partition]) {
    for p in volume_parts.iter().filter(|p| p.fs == Fs::Btrfs) {
        match btrfs_image::prepare::remove_stale(&p.device(), &p.fs_uuid) {
            Ok(0) => {}
            Ok(n) => eprintln!("{}: deleted {n} snapshots left over from previous backups", alias(p)),
            Err(e) => eprintln!("{}: cleaning up leftover snapshots: {e}", alias(p)),
        }
    }
}

/// At service startup: deletes leftover snapshots (interrupted backups, freshly restored
/// system), if no backup is running.
pub fn startup_cleanup() {
    let Ok(Some(_lock)) = flock(LOCK, true, false) else { return };
    match scan_disks(&Selection::WholeDevice) {
        Ok(((_, parts), _)) => remove_stale_snapshots(&parts),
        Err(e) => eprintln!("cleaning up leftover snapshots: {e}"),
    }
}

/// Runs a backup. One backup at a time (even across processes).
pub fn run(sess: &Session, trigger: Trigger) -> Result<()> {
    let wait = matches!(trigger, Trigger::Server(_));
    let _lock = flock(LOCK, true, wait)?;
    // Locks of other backup jobs (`schedule.wait_locks`): wait until each is free.
    let mut _shared = Vec::new();
    for l in &crate::config::get().schedule.wait_locks {
        if Path::new(l).exists() {
            eprintln!("waiting for {l}");
        }
        _shared.push(flock(l, false, true)?);
    }

    crate::hooks::on_start();
    let started = now();
    let res = run_locked(sess, trigger);
    let last = LastRun::load();
    let status = match &res {
        Ok(()) if last.start >= started => last.status.clone(),
        Ok(()) => "complete".into(),
        Err(_) if last.start >= started && last.status != "running" => last.status.clone(),
        Err(_) => "error".into(),
    };
    crate::hooks::on_finish(&status);
    res
}

/// The backup itself, once the locks are held.
fn run_locked(sess: &Session, trigger: Trigger) -> Result<()> {
    let mut c = sess.connect()?;
    let tasks = sess.tasks(&mut c)?;
    let task = match &trigger {
        Trigger::Server(rt) => tasks.iter().find(|t| t.get("id").and_then(Value::as_int) == Some(rt.task_id)),
        Trigger::Local { .. } => tasks.first(),
    }
    .and_then(TaskInfo::parse)
    .ok_or_else(|| Error::Protocol("no task for this device".into()))?;
    let (task_id, task_uuid, selection) = (task.id, task.uuid.clone(), task.selection.clone());
    eprintln!("source: {selection}");

    let ((disks, volume_parts), missed) = scan_disks(&selection)?;
    remove_stale_snapshots(&volume_parts);

    let base = sess.call(
        &mut c,
        "client_trigger",
        vec![("subaction", Value::str("get_base_version")), ("task_id", Value::Int(task_id))],
    )?;
    let base_version_id = get_int(&base, "base_version_id")?;
    let prev = previous_digests(&volume_parts, base_version_id);
    let raws = stream::RawTables::load(&disks, base_version_id);
    let is_cbt = prev.is_some() as u64;
    let (rt, upd) = match trigger {
        Trigger::Server(rt) => {
            let upd = sess.call(
                &mut c,
                "client_trigger",
                vec![
                    ("subaction", Value::str("update_backup_task")),
                    ("base_version_id", Value::Int(base_version_id)),
                    ("check_point_backup", Value::Int(0)),
                    ("is_cache", Value::Int(0)),
                    ("is_cbt", Value::Int(is_cbt)),
                    ("running_task_id", Value::Int(rt.running_task_id)),
                ],
            )?;
            (rt, upd)
        }
        Trigger::Local { trigger_type, trigger_event } => {
            let mut fields = vec![
                ("subaction", Value::str("add_backup_task")),
                ("base_version_id", Value::Int(base_version_id)),
                ("check_point_backup", Value::Int(0)),
                ("is_cache", Value::Int(0)),
                ("is_cbt", Value::Int(is_cbt)),
                ("task_id", Value::Int(task_id)),
                ("trigger_type", Value::Int(trigger_type)),
            ];
            if let Some(ev) = trigger_event {
                fields.push(("trigger_event", Value::Int(ev)));
            }
            let upd = sess.call(&mut c, "client_trigger", fields)?;
            let rt = RunningTask { running_task_id: get_int(&upd, "running_task_id")?, task_id, task_uuid, status: 1 };
            (rt, upd)
        }
    };
    let result_id = get_int(&upd, "result_id")?;
    let version_id = get_int(&upd, "version_id")?;
    let start_time = now();
    let mut last = LastRun::load();
    last.start = start_time;
    last.status = "running".into();
    last.result_id = result_id;
    last.server_status = String::new();
    let _ = last.save();
    let run_name = format!("abb-{start_time}");
    let job = Job { sess, rt: rt.clone(), result_id, run_name: run_name.clone() };
    control::begin(rt.running_task_id, Some(task.schedule.clone()));
    match sess.call(&mut c, "get_backup_bandwidth", vec![]) {
        Ok(r) => control::set_bandwidth(r.get("backup_bandwidth").and_then(Value::as_int).unwrap_or(0)),
        Err(e) => eprintln!("get_backup_bandwidth: {e}"),
    }
    eprintln!(
        "backup {}: task {task_id}, running_task {} version {version_id} (base {base_version_id})",
        if is_cbt == 1 { "incremental" } else { "full" },
        rt.running_task_id
    );

    let outcome = run_inner(&job, &mut c, &task, start_time, &disks, &volume_parts, prev.as_deref(), &raws, version_id)
        .map(|complete| complete && !missed);
    let status = match &outcome {
        Ok(true) => "complete",
        Ok(false) => "partial",
        Err(_) if control::cancelled() => "cancel",
        Err(_) => "error",
    };
    control::end();
    if let Err(e) = &outcome {
        eprintln!("backup failed: {e}");
    }
    last.end = now();
    last.status = status.into();
    if outcome.is_ok() {
        last.last_ok_start = start_time;
    }
    let _ = last.save();
    let mut c = sess.connect()?;
    sess.call(
        &mut c,
        "backup",
        vec![
            ("subaction", Value::str("check_complete")),
            ("is_cbt", Value::Int(is_cbt)),
            ("result_id", Value::Int(result_id)),
            ("running_task_id", Value::Int(rt.running_task_id)),
            ("status", Value::str(status)),
        ],
    )?;
    eprintln!("backup finished: {status}");
    outcome.map(|_| ())
}

/// Prepares the volumes (btrfs snapshots, maps, ext freeze) one after another; on error,
/// releases the ones already prepared.
fn prepare_volumes(volume_parts: &[Partition], run_name: &str, mut each: impl FnMut(&Partition, bool)) -> Result<Vec<Vol>> {
    let mut volumes = Vec::new();
    for p in volume_parts {
        each(p, false);
        match Vol::prepare(p, run_name) {
            Ok(v) => volumes.push(v),
            Err(e) => {
                volumes.into_iter().for_each(Vol::cleanup);
                return Err(e);
            }
        }
        each(p, true);
    }
    Ok(volumes)
}

/// `Ok(true)` complete backup, `Ok(false)` partial.
#[allow(clippy::too_many_arguments)]
fn run_inner(
    job: &Job,
    c: &mut Connection,
    task: &TaskInfo,
    start_time: u64,
    disks: &[(Disk, Vec<Interval>)],
    volume_parts: &[Partition],
    prev: Option<&[Digests]>,
    raws: &stream::RawTables,
    version_id: u64,
) -> Result<bool> {
    let sess = job.sess;
    job.log(c, LOG_DEVICE_INFO, None);
    scripts::run(job, c, &task.scripts, Hook::Pre, version_id)?;
    let t = Instant::now();
    let volumes = {
        let c = &mut *c;
        prepare_volumes(volume_parts, &job.run_name, |p, done| {
            job.log(c, if done { LOG_SNAPSHOT_DONE } else { LOG_SNAPSHOT_START }, Some(&alias(p)))
        })
    };
    // The post script runs right after the snapshots, whether they succeeded or not.
    let post = scripts::run(job, c, &task.scripts, Hook::Post, version_id);
    let mut volumes = volumes?;
    if let Err(e) = post {
        volumes.into_iter().for_each(Vol::cleanup);
        return Err(e);
    }
    // ext: blocks of unchanged files need not be read again.
    if prev.is_some() {
        for (p, v) in volume_parts.iter().zip(volumes.iter_mut()).filter(|(p, _)| matches!(p.fs, Fs::Ext(_))) {
            // The CBT table of this volume belongs to the base version (checked when loaded).
            let Ok((_, meta)) = Digests::load(&cbt_path(p)) else { continue };
            if let Some(sigs) = crate::volume::load_inodes(&cbt_path(p).with_extension("inodes"), meta.version_id) {
                let n = v.plan_skip(&sigs);
                eprintln!("{}: {n} blocks of 64 KiB belong to unchanged files, not read", alias(p));
            }
        }
    }
    let snapshot_time = t.elapsed();
    let result = (|| -> Result<bool> {
        let used: Vec<u64> = disks.iter().map(|(d, iv)| used_estimate(iv, &volumes, d)).collect();
        let mut plans: Vec<Vec<Piece>> = disks
            .iter()
            .map(|(d, iv)| match prev {
                Some(prev) => stream::plan_incremental(iv, &volumes, prev),
                None => stream::plan_full(d, iv, &volumes),
            })
            .collect();
        if let Some(prev) = prev {
            for (p, (v, old)) in volume_parts.iter().zip(volumes.iter().zip(prev)) {
                if let Some(r) = v.change_report(old) {
                    eprintln!("{}: {r}", alias(p));
                }
            }
        }
        if prev.is_some() {
            stream::settle_raw(&mut plans, disks, raws)?;
        }
        let transfer: Vec<u64> = plans
            .iter()
            .map(|p| p.iter().map(|x| if let Piece::Data { start, end, .. } = x { end - start } else { 0 }).sum())
            .collect();
        job.log(c, LOG_UPLOAD_SPEC_START, None);
        let spec = spec(sess, disks, &used, &transfer, prev.is_some(), start_time, &job.run_name);
        let mut spec_bytes = serde_json::to_string_pretty(&spec).unwrap().into_bytes();
        spec_bytes.push(b'\n');
        sess.call(
            c,
            "backup",
            vec![
                ("subaction", Value::str("update_device_spec")),
                ("enable_app_aware", Value::Int(0)),
                ("resume", Value::Int(0)),
                ("running_task_id", Value::Int(job.rt.running_task_id)),
                ("snapshot_info", Value::Buffer(spec_bytes)),
            ],
        )?;
        job.log(c, LOG_UPLOAD_SPEC_DONE, None);

        stream::upload(job, c, disks, &plans, &volumes, prev, snapshot_time, (task.compression, task.encryption), raws)?;
        raws.save(disks, version_id);
        // The version is on the server: the digests become the base for the next incremental.
        for (p, v) in volume_parts.iter().zip(&volumes) {
            let path = cbt_path(p);
            if let Err(e) = std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| v.save_digests(&path, version_id, p.offset)) {
                eprintln!("saving CBT for {}: {e}", alias(p));
            }
        }
        let complete = disks.iter().all(|(d, iv)| unsupported(d, iv).is_empty()) && volumes.iter().all(|v| !v.incomplete());
        Ok(complete)
    })();
    volumes.into_iter().for_each(Vol::cleanup);
    result
}

/// Offline test: prepares the disks' volumes, then reads, reassembles, verifies and compresses
/// everything a full backup would send, and reports the timings.
pub fn bench(names: Vec<String>, freeze_ext: bool, limit: Option<u64>, image: Option<&str>) -> Result<()> {
    let mut disks = Vec::new();
    let mut volume_parts = Vec::new();
    for n in names {
        let d = disk::scan(&n)?;
        let iv = plan(&d, &mut volume_parts, freeze_ext, &Selection::WholeDevice);
        disks.push((d, iv));
    }
    for (d, iv) in &disks {
        for p in unsupported(d, iv) {
            eprintln!("{}: excluded ({})", alias(p), p.fs.name());
        }
    }
    let t = Instant::now();
    let volumes = prepare_volumes(&volume_parts, &format!("abb-bench-{}", now()), |_, _| {})?;
    eprintln!("preparation and digests: {:.1}s ({} workers)", t.elapsed().as_secs_f64(), stream::workers());
    let t = Instant::now();
    let result = stream::bench(&disks, &volumes, limit, image);
    volumes.into_iter().for_each(Vol::cleanup);
    let (raw, packed) = result?;
    let s = t.elapsed().as_secs_f64();
    eprintln!(
        "read, verified and compressed {:.2} GiB → {:.2} GiB in {s:.1}s ({:.0} MiB/s)",
        raw as f64 / GIB,
        packed as f64 / GIB,
        raw as f64 / MIB as f64 / s
    );
    Ok(())
}
