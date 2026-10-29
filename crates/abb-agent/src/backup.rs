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
use crate::volume::Vol;
use abb_proto::transport::{Error, Result};
use abb_proto::{Connection, Value};
use btrfs_image::cbt::{CbtMeta, Digests};
use serde_json::{Value as Json, json};
use std::path::{Path, PathBuf};
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
    /// Which volumes of the disks are backed up, for `status` and the backup view, with the
    /// name of the setting in the console.
    pub fn volumes(&self) -> String {
        match self {
            Selection::WholeDevice => "all of them (console: entire device)".into(),
            Selection::System => "system volumes only (console: system volumes)".into(),
            Selection::Custom(paths) => format!("{} (console: custom volumes)", paths.join(", ")),
        }
    }

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

/// Disk intervals and volumes to prepare.
fn plan(d: &Disk, volumes: &mut Vec<Partition>, sel: &Selection) -> Vec<Interval> {
    let mut out = Vec::new();
    let mut pos = 0;
    let gap = |s: u64, e: u64| Interval { start: s, end: e, kind: 3, part: None, source: Source::Raw };
    for p in &d.partitions {
        if p.offset > pos {
            out.push(gap(pos, p.offset));
        }
        let vol = matches!(p.fs, Fs::Btrfs | Fs::Ext(_));
        let (kind, source) = if p.fs == Fs::Swap {
            (5, Source::Excluded(Skip::Swap))
        } else if !sel.selects(p) {
            (5, Source::Excluded(Skip::NotSelected))
        } else if unreadable(p).is_some() {
            (5, Source::Excluded(Skip::Unsupported))
        } else if vol {
            volumes.push(p.clone());
            (1, Source::Vol(volumes.len() - 1))
        } else if matches!(p.fs, Fs::None | Fs::Vfat) {
            // A vfat in use cannot be frozen: checked for changes after it is read.
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

/// Why a partition that would be copied cannot be read consistently, if it cannot: an unknown
/// filesystem someone may be writing to, or an ext held by the kernel but not mounted here (it
/// cannot be frozen).
fn unreadable(p: &Partition) -> Option<&'static str> {
    match p.fs {
        Fs::None if p.in_use() => Some("unknown filesystem in use (mounted read-write, or held by the kernel)"),
        Fs::Ext(_) if p.mount_path.is_empty() && p.busy => Some("in use but not mounted here: it cannot be frozen"),
        _ => None,
    }
}

/// Digest of a whole partition, to tell whether it changed while it was read.
fn partition_digest(d: &Disk, p: &Partition) -> Result<u64> {
    let dev = btrfs_image::device::Device::open(&d.device())?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; (4 * MIB) as usize];
    let mut o = p.offset;
    while o < p.offset + p.length {
        let n = (p.offset + p.length - o).min(4 * MIB) as usize;
        dev.read_exact_at(&mut buf[..n], o)?;
        h.update(&buf[..n]);
        o += n as u64;
    }
    Ok(h.digest())
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
        "agent_version": abb_proto::transport::agent_build(),
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

/// Where the CBT table of a volume is kept, and what ties it to the volume. A table is used only
/// for the same server and task, the same image on the server (a disk keeps its image, see
/// `state::image_names`), the same partition and the same filesystem: applied to another volume
/// it would mark as unchanged blocks that the server then takes from a different base.
pub struct Table {
    pub path: PathBuf,
    pub owner: String,
    /// Table of agent 0.2.13 and earlier (`cbt/<fs uuid>.cbt`, without owner): removed after the
    /// next backup.
    legacy: PathBuf,
    /// The legacy table may be the base: no other volume of this backup has the same filesystem
    /// UUID and the disk has a stable id, so its image is the one it was saved for.
    legacy_ok: bool,
    /// The file the base table was read from.
    loaded: Option<PathBuf>,
}

/// The tables of `volume_parts`, for task `task_uuid` of server `ds_id`.
fn tables(ds_id: &str, task_uuid: &str, disks: &[(Disk, Vec<Interval>)], volume_parts: &[Partition]) -> Vec<Table> {
    let dir = Path::new(crate::state::STATE_DIR).join("cbt");
    (0..volume_parts.len())
        .map(|v| {
            let p = &volume_parts[v];
            let d = disks
                .iter()
                .find(|(_, iv)| iv.iter().any(|i| i.source == Source::Vol(v)))
                .map(|(d, _)| d)
                .expect("every volume is on a disk");
            let key = format!("{}-p{}-{}", d.image, p.number, p.fs_uuid);
            let unique = volume_parts.iter().filter(|q| q.fs_uuid == p.fs_uuid).count() == 1;
            Table {
                path: dir.join(format!("{key}.cbt")),
                owner: format!("{ds_id}/{task_uuid}/{key}"),
                legacy: dir.join(format!("{}.cbt", p.fs_uuid)),
                legacy_ok: unique && disk::stable_id(&d.name).is_some(),
                loaded: None,
            }
        })
        .collect()
}

/// Why a saved table cannot be the base of this backup (`None` if it can): it must belong to
/// `owner` (empty for a legacy table), to `base_version_id` and to a partition at `offset`.
fn table_mismatch(meta: &CbtMeta, base_version_id: u64, offset: u64, owner: &str) -> Option<String> {
    if meta.owner != owner {
        return Some(if meta.owner.is_empty() { "has no owner".into() } else { format!("belongs to {}", meta.owner) });
    }
    if meta.version_id != base_version_id {
        return Some(format!("is from version {} (base {base_version_id})", meta.version_id));
    }
    if meta.partition_offset != offset {
        return Some(format!("is for a partition at offset {}", meta.partition_offset));
    }
    None
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

/// CBT tables of the previous backup, by volume: `None` for a volume whose table does not match
/// `base_version_id` or the volume (a new disk or volume), which is sent in full within the
/// incremental. `None` overall when no volume has one: a full backup.
fn previous_digests(volume_parts: &[Partition], tables: &mut [Table], base_version_id: u64) -> Option<Vec<Option<Digests>>> {
    if base_version_id == 0 || volume_parts.is_empty() {
        return None;
    }
    let out: Vec<Option<Digests>> = volume_parts
        .iter()
        .zip(tables.iter_mut())
        .map(|(p, t)| {
            let mut candidates = vec![(t.path.clone(), t.owner.as_str())];
            if t.legacy_ok && !t.path.exists() {
                candidates.push((t.legacy.clone(), ""));
            }
            let mut why = "no CBT table".to_string();
            for (path, owner) in candidates {
                match Digests::load(&path) {
                    Ok((d, meta)) => match table_mismatch(&meta, base_version_id, p.offset, owner) {
                        None => {
                            t.loaded = Some(path);
                            return Some(d);
                        }
                        Some(m) => why = format!("CBT table {m}"),
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => why = format!("CBT table: {e}"),
                }
            }
            eprintln!("{}: {why}: sent in full", alias(p));
            None
        })
        .collect();
    out.iter().any(Option::is_some).then_some(out)
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

/// A backup holds the lock, in this process or another (`abb-agent backup` by hand).
pub fn in_progress() -> bool {
    matches!(flock(LOCK, false, false), Err(Error::Protocol(_)))
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
    let mut skipped = Vec::new();
    for n in names {
        let d = match disk::scan(&n) {
            Ok(d) => d,
            Err(e) if !explicit => {
                let has_parts = disk::has_partitions(&n);
                // An empty disk is not a problem: no partition table, nothing to keep.
                let why = if has_parts { e.to_string() } else { "no partitions".into() };
                eprintln!("{n}: not backed up: {why}");
                skipped.push(format!("{n} ({why})"));
                missed |= has_parts;
                continue;
            }
            Err(e) => return Err(Error::Protocol(format!("{n}: {e}"))),
        };
        let mut parts = Vec::new();
        let iv = plan(&d, &mut parts, sel);
        for p in unsupported(&d, &iv) {
            let why = unreadable(p).map_or_else(|| format!("{} not supported", p.fs.name()), str::to_string);
            eprintln!("{}: left out ({why}): partial backup", alias(p));
        }
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
    let names: Vec<String> = disks.iter().map(|(d, _)| crate::disk::label(d)).collect();
    crate::ui::row("Disks", &names.join(", "));
    if !skipped.is_empty() {
        crate::ui::row("Not included", &skipped.join(", "));
    }
    crate::ui::row("Volumes", &sel.volumes());
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

/// At service startup: thaws filesystems an interrupted backup left frozen and deletes leftover
/// snapshots (interrupted backups, freshly restored system), if no backup is running.
pub fn startup_cleanup() {
    let Ok(Some(_lock)) = flock(LOCK, true, false) else { return };
    crate::volume::thaw_leftovers();
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
            crate::ui::busy(&format!("waiting for {l} (another backup job)"));
        }
        _shared.push(flock(l, false, true)?);
    }
    // A backup killed while a filesystem was frozen (no service restart since).
    crate::volume::thaw_leftovers();

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
    let detail = match &res {
        Ok(()) => String::new(),
        Err(e) => e.to_string(),
    };
    crate::ui::finish(&status, &detail);
    res
}

/// The backup itself, once the locks are held.
fn run_locked(sess: &Session, trigger: Trigger) -> Result<()> {
    let mut c = sess.connect()?;
    let tasks = sess.tasks(&mut c)?;
    let raw = match &trigger {
        Trigger::Server(rt) => tasks.iter().find(|t| t.get("id").and_then(Value::as_int) == Some(rt.task_id)),
        Trigger::Local { .. } => tasks.first(),
    };
    let task_name = raw.and_then(|t| t.get("name")).and_then(Value::as_str).unwrap_or("").to_string();
    let task = raw.and_then(TaskInfo::parse).ok_or_else(|| Error::Protocol("no task for this device".into()))?;
    let (task_id, task_uuid, selection) = (task.id, task.uuid.clone(), task.selection.clone());
    eprintln!("source: {selection}");
    crate::ui::row("Server", &format!("{}:{} · user {}", sess.state.server, sess.state.port, sess.state.username));
    crate::ui::row("Task", &format!("#{task_id} {task_name}"));

    let ((disks, volume_parts), missed) = scan_disks(&selection)?;
    remove_stale_snapshots(&volume_parts);

    let base = sess.call(
        &mut c,
        "client_trigger",
        vec![("subaction", Value::str("get_base_version")), ("task_id", Value::Int(task_id))],
    )?;
    let base_version_id = get_int(&base, "base_version_id")?;
    let mut tables = tables(&sess.state.ds_id, &task.uuid, &disks, &volume_parts);
    let prev = previous_digests(&volume_parts, &mut tables, base_version_id);
    let raws = stream::RawTables::load(&disks, base_version_id, &format!("{}/{}", sess.state.ds_id, task.uuid));
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
    crate::state::Progress::phase("preparing", 0, 0);
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
    crate::ui::gap();
    crate::ui::running(&if is_cbt == 1 {
        format!("incremental · version {version_id}, based on {base_version_id}")
    } else {
        format!("full · version {version_id}")
    });

    let check_complete = |status: &str| -> Result<()> {
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
        Ok(())
    };
    let outcome = run_inner(&job, &mut c, &task, start_time, &disks, &volume_parts, &tables, prev.as_deref(), &raws, version_id, missed, &check_complete);
    let status = match &outcome {
        Ok(true) => "complete",
        Ok(false) => "partial",
        Err(_) if control::cancelled() => "cancel",
        Err(_) => "error",
    };
    control::end();
    crate::state::Progress::clear();
    if let Err(e) = &outcome {
        eprintln!("backup failed: {e}");
    }
    // A successful backup is final only once the server confirmed it (`check_complete` in
    // `run_inner`): until then neither the result nor the CBT tables change.
    last.end = now();
    last.status = status.into();
    if outcome.is_ok() {
        last.last_ok_start = start_time;
    }
    if let Err(e) = last.save() {
        eprintln!("saving the result: {e}");
    }
    if outcome.is_err()
        && let Err(e) = check_complete(status)
    {
        eprintln!("check_complete: {e}");
    }
    eprintln!("backup finished: {status}");
    outcome.map(|_| ())
}

/// The server's result says it did not store the version. Only failures count: before
/// `check_complete` the result of a version still being written has no final status yet (its
/// value then is not documented), so anything else, empty included, cannot be read as a refusal;
/// `check_complete` is what confirms the version.
fn rejected(status: &str) -> bool {
    ["fail", "error", "cancel"].iter().any(|f| status.starts_with(f))
}

/// The server answers the stream's `close` even when it failed to store it (it drains the
/// rest after a write error): its result for the version says whether it took it.
fn server_accepted(job: &Job, c: &mut Connection) -> Result<()> {
    let r = job.sess.call(c, "get_running_task_result", vec![("result_id", Value::Int(job.result_id))])?;
    let status = r.get("running_task_result").and_then(|t| t.get("status")).and_then(Value::as_str).unwrap_or("");
    if rejected(status) {
        return Err(Error::Protocol(format!("the server did not store the version (status {status}): see its log in the ABB console")));
    }
    Ok(())
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
    tables: &[Table],
    prev: Option<&[Option<Digests>]>,
    raws: &stream::RawTables,
    version_id: u64,
    missed: bool,
    check_complete: &dyn Fn(&str) -> Result<()>,
) -> Result<bool> {
    let sess = job.sess;
    job.log(c, LOG_DEVICE_INFO, None);
    scripts::run(job, c, &task.scripts, Hook::Pre, version_id)?;
    let t = Instant::now();
    let volumes = {
        let c = &mut *c;
        prepare_volumes(volume_parts, &job.run_name, |p, done| {
            if !done {
                crate::ui::busy(&format!("{}: snapshot and changed blocks", alias(p)));
            }
            job.log(c, if done { LOG_SNAPSHOT_DONE } else { LOG_SNAPSHOT_START }, Some(&alias(p)))
        })
    };
    if let Ok(volumes) = &volumes {
        for (p, v) in volume_parts.iter().zip(volumes) {
            crate::ui::step(crate::ui::Mark::Ok, &format!("{}  {} · {}", alias(p), p.fs.name(), v.summary()));
        }
    }
    // The post script runs right after the snapshots, whether they succeeded or not.
    let post = scripts::run(job, c, &task.scripts, Hook::Post, version_id);
    let mut volumes = volumes?;
    if let Err(e) = post {
        volumes.into_iter().for_each(Vol::cleanup);
        return Err(e);
    }
    // ext: blocks of unchanged files need not be read again.
    if let Some(prev) = prev {
        for (((p, v), _), t) in volume_parts.iter().zip(volumes.iter_mut()).zip(prev).zip(tables).filter(|(((p, _), old), _)| matches!(p.fs, Fs::Ext(_)) && old.is_some()) {
            // The CBT table of this volume belongs to the base version (checked when loaded).
            let Some(src) = &t.loaded else { continue };
            let Ok((_, meta)) = Digests::load(src) else { continue };
            if let Some(sigs) = crate::volume::load_inodes(&src.with_extension("inodes"), meta.version_id) {
                let n = v.plan_skip(&sigs);
                eprintln!("{}: {n} blocks of 64 KiB belong to unchanged files, not read", alias(p));
            }
        }
    }
    let snapshot_time = t.elapsed();
    let result = (|| -> Result<bool> {
        // vfat partitions in use, copied as they are: their digest before anything is read and
        // after everything is, to tell whether the copy is consistent.
        let live: Vec<(&Disk, &Partition)> = disks
            .iter()
            .flat_map(|(d, iv)| iv.iter().filter(|i| i.source == Source::Raw).filter_map(move |i| Some((d, i.part.as_ref()?))))
            .filter(|(_, p)| p.fs == Fs::Vfat && p.in_use())
            .collect();
        let before = live.iter().map(|(d, p)| partition_digest(d, p)).collect::<Result<Vec<u64>>>()?;
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
                if let Some(r) = old.as_ref().and_then(|old| v.change_report(old)) {
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
        let to_read: u64 = transfer.iter().sum();
        crate::ui::step(crate::ui::Mark::Ok, &format!("{} to read{}", crate::ui::size(to_read), if prev.is_some() { " (blocks changed since the base version)" } else { "" }));
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
        let mut consistent = true;
        for ((d, p), h) in live.iter().zip(before) {
            if partition_digest(d, p)? != h {
                eprintln!("{}: changed while the backup read it (vfat cannot be frozen): its copy may be inconsistent, partial backup", alias(p));
                consistent = false;
            }
        }
        server_accepted(job, c)?;
        for (p, v) in volume_parts.iter().zip(&volumes) {
            if let Some(n) = v.changed_while_read().filter(|&n| n > 0) {
                eprintln!("{}: {n} files changed while the backup read them: their content in this version may be inconsistent (partial backup), the next backup reads them again", alias(p));
                consistent = false;
            }
        }
        let complete = !missed
            && consistent
            && disks.iter().all(|(d, iv)| unsupported(d, iv).is_empty())
            && volumes.iter().all(|v| !v.incomplete());
        check_complete(if complete { "complete" } else { "partial" })?;
        // The server took the version: the digests become the base for the next incremental.
        raws.save(disks, version_id);
        for ((p, v), t) in volume_parts.iter().zip(&volumes).zip(tables) {
            match std::fs::create_dir_all(t.path.parent().unwrap()).and_then(|_| v.save_digests(&t.path, version_id, p.offset, &t.owner)) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&t.legacy);
                    let _ = std::fs::remove_file(t.legacy.with_extension("inodes"));
                }
                Err(e) => eprintln!("saving CBT for {}: {e}", alias(p)),
            }
        }
        Ok(complete)
    })();
    volumes.into_iter().for_each(Vol::cleanup);
    result
}

/// Offline test: prepares the disks' volumes, then reads, reassembles, verifies and compresses
/// everything a full backup would send, and reports the timings.
pub fn bench(names: Vec<String>, limit: Option<u64>, image: Option<&str>) -> Result<()> {
    let mut disks = Vec::new();
    let mut volume_parts = Vec::new();
    for n in names {
        let d = disk::scan(&n)?;
        let iv = plan(&d, &mut volume_parts, &Selection::WholeDevice);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn part(name: &str, number: u32, offset: u64, fs: Fs, uuid: &str) -> Partition {
        Partition {
            name: name.into(),
            number,
            offset,
            length: 1 << 30,
            type_guid: String::new(),
            fs,
            fs_uuid: uuid.into(),
            mount_path: String::new(),
            mounts: vec![],
            read_only: true,
            busy: false,
        }
    }

    fn disk(name: &str, parts: Vec<Partition>) -> Disk {
        Disk { name: name.into(), image: name.into(), size: 4 << 30, sector_size: 512, layout: "GPT".into(), partitions: parts }
    }

    #[test]
    fn partitions_in_use_are_copied_only_if_consistent() {
        let mut esp = part("zzsda1", 1, 1 << 20, Fs::Vfat, "AB-CD");
        let mut ntfs = part("zzsda2", 2, 2 << 20, Fs::None, "");
        let mut ext = part("zzsda3", 3, 3 << 20, Fs::Ext(4), "u");
        let d = disk("zzsda", vec![esp.clone(), ntfs.clone(), ext.clone()]);
        let sources = |d: &Disk| plan(d, &mut Vec::new(), &Selection::WholeDevice).iter().filter(|i| i.part.is_some()).map(|i| i.source).collect::<Vec<_>>();
        // Unmounted: copied raw, ext as a volume.
        assert_eq!(sources(&d), [Source::Raw, Source::Raw, Source::Vol(0)]);
        // Mounted read-write: vfat still copied (checked afterwards), the unknown filesystem left
        // out; held by the kernel elsewhere: the same, and ext cannot be frozen.
        (esp.mount_path, esp.read_only) = ("/boot".into(), false);
        (ntfs.mount_path, ntfs.read_only) = ("/data".into(), false);
        ext.busy = true;
        let d = disk("zzsda", vec![esp.clone(), ntfs.clone(), ext.clone()]);
        assert_eq!(sources(&d), [Source::Raw, Source::Excluded(Skip::Unsupported), Source::Excluded(Skip::Unsupported)]);
        // Mounted read-only: copied.
        ntfs.read_only = true;
        let d = disk("zzsda", vec![esp, ntfs, ext]);
        assert_eq!(sources(&d)[1], Source::Raw);
    }

    #[test]
    fn last_read_of_each_volume() {
        let d = disk("zzsda", vec![part("zzsda1", 1, 1 << 20, Fs::Ext(4), "a"), part("zzsda2", 2, 3 << 20, Fs::Ext(4), "b")]);
        let iv = vec![
            Interval { start: 0, end: 1 << 20, kind: 3, part: None, source: Source::Raw },
            Interval { start: 1 << 20, end: 3 << 20, kind: 1, part: Some(d.partitions[0].clone()), source: Source::Vol(0) },
            Interval { start: 3 << 20, end: 4 << 20, kind: 1, part: Some(d.partitions[1].clone()), source: Source::Vol(1) },
            Interval { start: 4 << 20, end: 5 << 20, kind: 1, part: None, source: Source::Vol(2) },
        ];
        let disks = vec![(d, iv)];
        // The area before the volumes, volume 0 twice, then a piece across both volumes.
        let jobs = vec![(0, 0, 1 << 20, None), (0, 1 << 20, 2 << 20, None), (0, 2 << 20, 3 << 20, None), (0, 0x2ff000, 0x301000, None)];
        assert_eq!(stream::last_reads(&disks, &jobs, 3), [Some(3), Some(3), None]);
        assert_eq!(stream::last_reads(&disks, &jobs[..3], 3), [Some(2), None, None]);
    }

    #[test]
    fn server_refusals() {
        for s in ["failure", "fail", "error", "cancel", "canceled"] {
            assert!(rejected(s), "{s}");
        }
        for s in ["", "complete", "partial", "success", "running", "backing_up"] {
            assert!(!rejected(s), "{s}");
        }
    }

    #[test]
    fn cbt_table_must_match_owner_version_and_offset() {
        let d = Digests::from_table(vec![1, 2]);
        let meta = CbtMeta::new(7, 0, 1 << 20, &d).owned("ds/task/vda-p2-u");
        assert_eq!(table_mismatch(&meta, 7, 1 << 20, "ds/task/vda-p2-u"), None);
        // Another image (disks renamed), another server or task, a legacy table.
        assert!(table_mismatch(&meta, 7, 1 << 20, "ds/task/vdb-p2-u").is_some());
        assert!(table_mismatch(&meta, 7, 1 << 20, "ds2/task/vda-p2-u").is_some());
        assert!(table_mismatch(&CbtMeta::new(7, 0, 1 << 20, &d), 7, 1 << 20, "ds/task/vda-p2-u").is_some());
        assert_eq!(table_mismatch(&CbtMeta::new(7, 0, 1 << 20, &d), 7, 1 << 20, ""), None);
        assert!(table_mismatch(&meta, 8, 1 << 20, "ds/task/vda-p2-u").is_some());
        assert!(table_mismatch(&meta, 7, 2 << 20, "ds/task/vda-p2-u").is_some());
    }

    #[test]
    fn clones_with_the_same_uuid_get_tables_of_their_own() {
        let mut disks = Vec::new();
        let mut parts = Vec::new();
        for name in ["zzvda", "zzvdb"] {
            let d = disk(name, vec![part(&format!("{name}2"), 2, 1 << 20, Fs::Ext(4), "same")]);
            let mut mine = Vec::new();
            let iv: Vec<Interval> = plan(&d, &mut mine, &Selection::WholeDevice)
                .into_iter()
                .map(|mut i| {
                    if let Source::Vol(v) = i.source {
                        i.source = Source::Vol(parts.len() + v);
                    }
                    i
                })
                .collect();
            parts.extend(mine);
            disks.push((d, iv));
        }
        let t = tables("ds", "task", &disks, &parts);
        assert_eq!(t.len(), 2);
        assert_ne!(t[0].path, t[1].path);
        assert_eq!(t[0].owner, "ds/task/zzvda-p2-same");
        assert_eq!(t[1].owner, "ds/task/zzvdb-p2-same");
        // The legacy table (by UUID only) could belong to either: never used.
        assert!(!t[0].legacy_ok && !t[1].legacy_ok);
    }
}
