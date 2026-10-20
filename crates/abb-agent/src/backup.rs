//! Running a backup: disks and volumes, device spec, start and completion on the server;
//! the block stream is in `stream.rs`.

use crate::disk::{self, Disk, Fs, Partition};
use crate::host::now;
use crate::session::{RunningTask, Session, get_int};
use crate::state::LastRun;
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
    /// Excluded: not sent (swap, filesystems that are unsupported or cannot be read
    /// consistently).
    Excluded,
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
fn plan(d: &Disk, volumes: &mut Vec<Partition>, freeze_ext: bool) -> Vec<Interval> {
    let mut out = Vec::new();
    let mut pos = 0;
    let gap = |s: u64, e: u64| Interval { start: s, end: e, kind: 3, part: None, source: Source::Raw };
    for p in &d.partitions {
        if p.offset > pos {
            out.push(gap(pos, p.offset));
        }
        let vol = matches!(p.fs, Fs::Btrfs) || (matches!(p.fs, Fs::Ext(_)) && volume::ext_readable(p, freeze_ext));
        let (kind, source) = if vol {
            volumes.push(p.clone());
            (1, Source::Vol(volumes.len() - 1))
        } else if matches!(p.fs, Fs::None | Fs::Vfat) {
            (2, Source::Raw)
        } else {
            (5, Source::Excluded)
        };
        out.push(Interval { start: p.offset, end: p.offset + p.length, kind, part: Some(p.clone()), source });
        pos = p.offset + p.length;
    }
    if pos < d.size {
        out.push(gap(pos, d.size));
    }
    out
}

fn is_excluded(intervals: &[Interval], p: &Partition) -> bool {
    intervals.iter().any(|i| i.source == Source::Excluded && i.part.as_ref().is_some_and(|q| q.name == p.name))
}

/// Excluded volumes that contain data ("partial" backup); swap does not count.
fn unsupported<'a>(d: &'a Disk, intervals: &[Interval]) -> Vec<&'a Partition> {
    d.partitions.iter().filter(|p| p.fs != Fs::Swap && is_excluded(intervals, p)).collect()
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

fn spec(sess: &Session, disks: &[(Disk, Vec<Interval>)], used: &[u64], start_time: u64, run_name: &str) -> Json {
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
            if !p.mount_path.is_empty() && !excluded {
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
                        "is_system": true,
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
            "changed_length": d.size.to_string(),
            "excluded_length": excluded_length.to_string(),
            "image_name": d.image_name(),
            "length": d.size.to_string(),
            "original_used_length": used[n].to_string(),
            "transfer_length": used[n].to_string(),
            "transfer_length_4k": used[n].to_string(),
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
            "is_bootable": true,
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
                    Source::Excluded => false,
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
    Local { trigger_type: u64 },
}

/// `trigger_type` for `add_backup_task`: 2 for scheduled runs, as the official agent does,
/// 1 for manual runs.
pub const TRIGGER_SCHEDULE: u64 = 2;
pub const TRIGGER_MANUAL: u64 = 1;

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

fn scan_disks() -> Result<Scan> {
    let cfg = crate::config::get();
    let names = if cfg.disks.is_empty() { vec![disk::system_disk()?] } else { cfg.disks.clone() };
    let mut disks = Vec::new();
    let mut volume_parts = Vec::new();
    for n in names {
        let d = disk::scan(&n)?;
        let iv = plan(&d, &mut volume_parts, cfg.ext.freeze_mounted);
        disks.push((d, iv));
    }
    Ok((disks, volume_parts))
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
    match scan_disks() {
        Ok((_, parts)) => remove_stale_snapshots(&parts),
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

    let (disks, volume_parts) = scan_disks()?;
    remove_stale_snapshots(&volume_parts);

    let mut c = sess.connect()?;
    let (task_id, task_uuid) = match &trigger {
        Trigger::Server(rt) => (rt.task_id, rt.task_uuid.clone()),
        Trigger::Local { .. } => {
            let tasks = sess.tasks(&mut c)?;
            let t = tasks.first().ok_or_else(|| Error::Protocol("no task for this device".into()))?;
            (get_int(t, "id")?, t.get("task_uuid").and_then(Value::as_str).unwrap_or("").to_string())
        }
    };
    let base = sess.call(
        &mut c,
        "client_trigger",
        vec![("subaction", Value::str("get_base_version")), ("task_id", Value::Int(task_id))],
    )?;
    let base_version_id = get_int(&base, "base_version_id")?;
    let prev = previous_digests(&volume_parts, base_version_id);
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
        Trigger::Local { trigger_type } => {
            let upd = sess.call(
                &mut c,
                "client_trigger",
                vec![
                    ("subaction", Value::str("add_backup_task")),
                    ("base_version_id", Value::Int(base_version_id)),
                    ("check_point_backup", Value::Int(0)),
                    ("is_cache", Value::Int(0)),
                    ("is_cbt", Value::Int(is_cbt)),
                    ("task_id", Value::Int(task_id)),
                    ("trigger_type", Value::Int(trigger_type)),
                ],
            )?;
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
    let _ = last.save();
    let run_name = format!("abb-{start_time}");
    let job = Job { sess, rt: rt.clone(), result_id, run_name: run_name.clone() };
    eprintln!(
        "backup {}: task {task_id}, running_task {} version {version_id} (base {base_version_id})",
        if is_cbt == 1 { "incremental" } else { "full" },
        rt.running_task_id
    );

    let outcome = run_inner(&job, &mut c, start_time, &disks, &volume_parts, prev.as_deref(), version_id);
    let status = match &outcome {
        Ok(true) => "complete",
        Ok(false) => "partial",
        Err(_) => "error",
    };
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
fn run_inner(
    job: &Job,
    c: &mut Connection,
    start_time: u64,
    disks: &[(Disk, Vec<Interval>)],
    volume_parts: &[Partition],
    prev: Option<&[Digests]>,
    version_id: u64,
) -> Result<bool> {
    let sess = job.sess;
    job.log(c, LOG_DEVICE_INFO, None);
    let t = Instant::now();
    let volumes = {
        let c = &mut *c;
        prepare_volumes(volume_parts, &job.run_name, |p, done| {
            job.log(c, if done { LOG_SNAPSHOT_DONE } else { LOG_SNAPSHOT_START }, Some(&alias(p)))
        })?
    };
    let snapshot_time = t.elapsed();
    let result = (|| -> Result<bool> {
        let used: Vec<u64> = disks.iter().map(|(d, iv)| used_estimate(iv, &volumes, d)).collect();
        job.log(c, LOG_UPLOAD_SPEC_START, None);
        let spec = spec(sess, disks, &used, start_time, &job.run_name);
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

        let plans: Vec<Vec<Piece>> = disks
            .iter()
            .map(|(d, iv)| match prev {
                Some(prev) => stream::plan_incremental(iv, &volumes, prev),
                None => stream::plan_full(d, iv, &volumes),
            })
            .collect();
        stream::upload(job, c, disks, &plans, &volumes, prev, snapshot_time)?;
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
pub fn bench(names: Vec<String>, freeze_ext: bool, limit: Option<u64>) -> Result<()> {
    let mut disks = Vec::new();
    let mut volume_parts = Vec::new();
    for n in names {
        let d = disk::scan(&n)?;
        let iv = plan(&d, &mut volume_parts, freeze_ext);
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
    let result = stream::bench(&disks, &volumes, limit);
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
