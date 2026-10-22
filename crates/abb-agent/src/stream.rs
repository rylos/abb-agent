//! Block stream to the server (`backup/upload_block`): piece planning, parallel preparation
//! (read, reassembly, verification, LZ4) and in-order sending.

use crate::backup::{Interval, Job, LOG_VOLUME_DONE, LOG_VOLUME_START, Source, alias};
use crate::disk::Disk;
use crate::volume::Vol;
use abb_proto::transport::{Error, Result};
use abb_proto::{Connection, Value, request};
use btrfs_image::cbt::{self, Digests};
use btrfs_image::device::Device;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub const MIB: u64 = 1 << 20;
/// Maximum length of an "unused" message (the official agent sends up to ~2 GiB).
const MAX_UNUSED_RUN: u64 = 1 << 30;

/// A piece of the stream, in the order it must be sent.
pub enum Piece {
    /// Disk bytes in `[start, end)` to read, reassemble, verify and send. With `check` the
    /// piece is sent only if the digest computed while reading it differs from the previous
    /// backup's; otherwise it goes as "used, unchanged".
    Data { start: u64, end: u64, cbt: bool, check: Option<Check> },
    /// Piece without data.
    Run(Run),
    /// `log_report` entry for a volume (start or end).
    Log(u64, String),
}

/// How a piece read in full is compared with the previous backup.
#[derive(Clone, Copy)]
pub enum Check {
    /// Block of an ext volume (volume, block).
    Vol(usize, usize),
    /// Area outside the volumes (EFI system partition, partition tables, free space): digest of
    /// the piece, by disk offset, in [`RawTables`].
    Raw,
}

/// Digests of the areas outside the volumes, by disk and offset: those of the previous backup
/// and those computed by this one.
#[derive(Default)]
pub struct RawTables {
    pub prev: Vec<std::collections::HashMap<u64, u64>>,
    pub cur: Mutex<Vec<std::collections::HashMap<u64, u64>>>,
}

impl RawTables {
    fn path(d: &Disk) -> std::path::PathBuf {
        std::path::Path::new(crate::state::STATE_DIR).join("cbt").join(format!("{}.raw", d.image_name()))
    }

    /// The previous backup's digests, if they belong to `base_version_id`.
    pub fn load(disks: &[(Disk, Vec<Interval>)], base_version_id: u64) -> RawTables {
        let prev = disks
            .iter()
            .map(|(d, _)| {
                let j: Option<serde_json::Value> = std::fs::read(Self::path(d)).ok().and_then(|b| serde_json::from_slice(&b).ok());
                match j {
                    Some(j) if j["version_id"].as_u64() == Some(base_version_id) => j["blocks"]
                        .as_object()
                        .map(|m| m.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v.as_u64()?))).collect())
                        .unwrap_or_default(),
                    _ => Default::default(),
                }
            })
            .collect();
        RawTables { prev, cur: Mutex::new(vec![Default::default(); disks.len()]) }
    }

    /// Saves this backup's digests as the base for the next one.
    pub fn save(&self, disks: &[(Disk, Vec<Interval>)], version_id: u64) {
        let cur = self.cur.lock().unwrap();
        for ((d, _), blocks) in disks.iter().zip(cur.iter()) {
            let m: serde_json::Map<String, serde_json::Value> = blocks.iter().map(|(k, v)| (k.to_string(), (*v).into())).collect();
            let j = serde_json::json!({ "version_id": version_id, "blocks": m });
            if let Err(e) = std::fs::write(Self::path(d), j.to_string()) {
                eprintln!("saving the digests of {}: {e}", d.name);
            }
        }
    }
}

/// Before an incremental: reads and digests the areas outside the volumes, and turns the
/// pieces unchanged since the previous backup into "used, unchanged" ones, so that the total
/// announced to the server (the console's progress bar) counts only what is sent.
pub fn settle_raw(plans: &mut [Vec<Piece>], disks: &[(Disk, Vec<Interval>)], raws: &RawTables) -> Result<()> {
    for (n, plan) in plans.iter_mut().enumerate() {
        if raws.prev[n].is_empty() {
            continue;
        }
        let dev = Device::open(&disks[n].0.device())?;
        let mut buf = Vec::new();
        for p in plan.iter_mut() {
            let Piece::Data { start, end, check: Some(Check::Raw), .. } = *p else { continue };
            buf.resize((end - start) as usize, 0);
            dev.read_exact_at(&mut buf, start)?;
            let h = xxhash_rust::xxh3::xxh3_64(&buf).max(1);
            if raws.prev[n].get(&start) == Some(&h) {
                raws.cur.lock().unwrap()[n].insert(start, h);
                *p = Piece::Run(Run { start, end, used: true, changed: false, cbt: true });
            }
        }
    }
    Ok(())
}

/// Message without data (possibly extended over contiguous pieces with the same flags).
#[derive(Clone, Copy)]
pub struct Run {
    start: u64,
    end: u64,
    used: bool,
    changed: bool,
    cbt: bool,
}

/// Plan for a full backup: 1 MiB windows; those with no used bytes become "unused"
/// messages.
pub fn plan_full(d: &Disk, intervals: &[Interval], volumes: &[Vol]) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut announced = vec![false; intervals.len()];
    let mut w = 0;
    while w < d.size {
        let we = (w + MIB).min(d.size);
        let mut used = false;
        let mut done = Vec::new();
        for (k, i) in intervals.iter().enumerate() {
            if i.start >= we || i.end <= w {
                continue;
            }
            if let (Some(p), true) = (&i.part, i.kind == 1 || i.kind == 2) {
                if !announced[k] {
                    announced[k] = true;
                    out.push(Piece::Log(LOG_VOLUME_START, alias(p)));
                }
                if we >= i.end {
                    done.push(alias(p));
                }
            }
            used |= match i.source {
                Source::Raw => true,
                Source::Excluded(_) => false,
                Source::Vol(v) => {
                    let off = i.part.as_ref().unwrap().offset;
                    volumes[v].any_used(w.max(i.start) - off, we.min(i.end) - off)
                }
            };
        }
        let raw = intervals.iter().filter(|i| i.start < we && i.end > w).all(|i| matches!(i.source, Source::Raw));
        out.push(if used {
            Piece::Data { start: w, end: we, cbt: false, check: raw.then_some(Check::Raw) }
        } else {
            Piece::Run(Run { start: w, end: we, used: false, changed: true, cbt: false })
        });
        out.extend(done.into_iter().map(|a| Piece::Log(LOG_VOLUME_DONE, a)));
        w = we;
    }
    out
}

/// Plan for an incremental: 64 KiB blocks with `is_interval_cbt:1` inside volumes; the rest
/// of the disk (GPT tables, ESP, ...) is sent again in full, as the official agent does, and so
/// are volumes with no CBT table of the base (`None` in `prev`: a new disk or volume).
pub fn plan_incremental(intervals: &[Interval], volumes: &[Vol], prev: &[Option<Digests>]) -> Vec<Piece> {
    let mut out = Vec::new();
    for i in intervals {
        let logged = i.part.as_ref().filter(|_| i.kind == 1 || i.kind == 2).map(alias);
        if let Some(a) = &logged {
            out.push(Piece::Log(LOG_VOLUME_START, a.clone()));
        }
        match i.source {
            Source::Excluded(_) => {
                out.push(Piece::Run(Run { start: i.start, end: i.end, used: false, changed: false, cbt: false }));
            }
            Source::Raw => {
                let mut w = i.start;
                while w < i.end {
                    let we = ((w / MIB + 1) * MIB).min(i.end);
                    out.push(Piece::Data { start: w, end: we, cbt: false, check: Some(Check::Raw) });
                    w = we;
                }
            }
            Source::Vol(v) if prev[v].is_none() => {
                // As in a full backup: used MiBs sent, free ones described as unused.
                let off = i.part.as_ref().unwrap().offset;
                let mut w = i.start;
                while w < i.end {
                    let we = ((w / MIB + 1) * MIB).min(i.end);
                    out.push(if volumes[v].any_used(w - off, we - off) {
                        Piece::Data { start: w, end: we, cbt: false, check: None }
                    } else {
                        Piece::Run(Run { start: w, end: we, used: false, changed: true, cbt: false })
                    });
                    w = we;
                }
            }
            Source::Vol(v) => {
                let (vol, old) = (&volumes[v], prev[v].as_ref().unwrap());
                let blocks = (i.end - i.start).div_ceil(cbt::BLOCK);
                let per_mib = MIB / cbt::BLOCK;
                let btrfs = vol.changed(old, 0).is_some();
                let (mut whole, mut freed) = (false, false);
                for k in 0..blocks {
                    let s = i.start + k * cbt::BLOCK;
                    let e = (s + cbt::BLOCK).min(i.end);
                    // A MiB holding data is described as a whole, as by a full backup: what did
                    // not change in it goes as "used, unchanged", without splitting it at every
                    // free 64 KiB block (hundreds of thousands of messages on a big fragmented
                    // btrfs). Freed blocks keep their old content, which nothing reads.
                    if k % per_mib == 0 {
                        let mib = k..(k + per_mib).min(blocks);
                        whole = btrfs && mib.clone().any(|b| vol.block_used(b as usize));
                        // A free MiB gets one "changed" flag (data there before), so that free
                        // MiBs in a row make a single message.
                        freed = btrfs && mib.clone().any(|b| old.used(b as usize));
                    }
                    let k = k as usize;
                    if whole && !(vol.block_used(k) && vol.changed(old, k) == Some(true)) {
                        out.push(Piece::Run(Run { start: s, end: e, used: true, changed: false, cbt: true }));
                        continue;
                    }
                    out.push(if !vol.block_used(k) {
                        Piece::Run(Run { start: s, end: e, used: false, changed: if btrfs { freed } else { old.used(k) }, cbt: true })
                    } else {
                        match vol.changed(old, k) {
                            Some(false) => Piece::Run(Run { start: s, end: e, used: true, changed: false, cbt: true }),
                            Some(true) => Piece::Data { start: s, end: e, cbt: true, check: None },
                            None if vol.skip_block(old, k) => Piece::Run(Run { start: s, end: e, used: true, changed: false, cbt: true }),
                            None => Piece::Data { start: s, end: e, cbt: true, check: Some(Check::Vol(v, k)) },
                        }
                    });
                }
            }
        }
        if let Some(a) = logged {
            out.push(Piece::Log(LOG_VOLUME_DONE, a));
        }
    }
    merge_changed(out)
}

/// Joins consecutive changed 64 KiB blocks into pieces of up to 1 MiB (never across a MiB
/// boundary): fewer messages for the server. Unchanged blocks are never sent again: an
/// incremental costs the server mostly the bytes it writes (joining across them tripled the
/// data for the same time).
fn merge_changed(pieces: Vec<Piece>) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::with_capacity(pieces.len());
    for p in pieces {
        if let Piece::Data { start, end, cbt: true, check: None } = p
            && let Some(Piece::Data { start: ps, end: pe, cbt: true, check: None }) = out.last_mut()
            && *pe == start
            && *ps / MIB == (end - 1) / MIB
        {
            *pe = end;
            continue;
        }
        out.push(p);
    }
    out
}

/// Disk bytes in `[start, end)` as they go into the image: read from the device, reassembled
/// and verified for volumes, zeroed for excluded areas.
fn build(dev: &Device, intervals: &[Interval], volumes: &[Vol], start: u64, end: u64) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; (end - start) as usize];
    dev.read_exact_at(&mut buf, start)?;
    for i in intervals.iter().filter(|i| i.start < end && i.end > start) {
        let (s, e) = (start.max(i.start), end.min(i.end));
        let seg = &mut buf[(s - start) as usize..(e - start) as usize];
        match i.source {
            Source::Raw => {}
            Source::Excluded(_) => seg.fill(0),
            Source::Vol(v) => {
                let p = i.part.as_ref().unwrap();
                volumes[v].fill(s - p.offset, seg).map_err(|e| Error::Protocol(format!("volume {}: {e}", alias(p))))?;
            }
        }
    }
    Ok(buf)
}

/// A ready data piece: compressed, with its read time. Empty `packed` = unchanged.
pub struct Ready {
    packed: Vec<u8>,
    len: u64,
    unchanged: bool,
    read_time: Duration,
}

struct Queue<T> {
    done: BTreeMap<usize, Result<T>>,
    /// First result not yet delivered.
    consumed: usize,
    abort: bool,
}

/// Preparation threads: `performance.workers` from the configuration, or one per CPU (at most
/// 8). `ABB_WORKERS` overrides it (for measurements).
pub fn workers() -> usize {
    if let Some(n) = std::env::var("ABB_WORKERS").ok().and_then(|v| v.parse::<usize>().ok()) {
        return n.max(1);
    }
    match crate::config::get().performance.workers {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8),
        n => n,
    }
}

/// Computes `produce(0..count)` on multiple threads and passes `consume` a function that
/// returns the results in order. Workers stay at most a few pieces ahead of the consumer.
fn parallel_ordered<T: Send, R>(
    count: usize,
    produce: impl Fn(usize) -> Result<T> + Sync,
    consume: impl FnOnce(&mut dyn FnMut() -> Result<T>) -> Result<R>,
) -> Result<R> {
    let nworkers = workers();
    let lookahead = 4 * nworkers;
    let next = AtomicUsize::new(0);
    let queue = Mutex::new(Queue { done: BTreeMap::new(), consumed: 0, abort: false });
    let cv = Condvar::new();
    std::thread::scope(|scope| {
        for _ in 0..nworkers {
            scope.spawn(|| {
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= count {
                        return;
                    }
                    {
                        let mut q = queue.lock().unwrap();
                        while !q.abort && idx >= q.consumed + lookahead {
                            q = cv.wait(q).unwrap();
                        }
                        if q.abort {
                            return;
                        }
                    }
                    let r = produce(idx);
                    let mut q = queue.lock().unwrap();
                    q.done.insert(idx, r);
                    cv.notify_all();
                }
            });
        }
        let mut idx = 0;
        let mut next_result = || -> Result<T> {
            let mut q = queue.lock().unwrap();
            loop {
                if let Some(r) = q.done.remove(&idx) {
                    idx += 1;
                    q.consumed = idx;
                    cv.notify_all();
                    return r;
                }
                if idx >= count {
                    return Err(Error::Protocol("no more results".into()));
                }
                q = cv.wait(q).unwrap();
            }
        };
        let result = consume(&mut next_result);
        let mut q = queue.lock().unwrap();
        q.abort = true;
        cv.notify_all();
        result
    })
}

/// Data jobs of the plan: (disk, start, end, check).
type DataJob = (usize, u64, u64, Option<Check>);

fn data_jobs(plans: &[Vec<Piece>]) -> Vec<DataJob> {
    plans
        .iter()
        .enumerate()
        .flat_map(|(n, plan)| {
            plan.iter().filter_map(move |p| match p {
                Piece::Data { start, end, check, .. } => Some((n, *start, *end, *check)),
                _ => None,
            })
        })
        .collect()
}

fn produce(
    devs: &[Device],
    disks: &[(Disk, Vec<Interval>)],
    volumes: &[Vol],
    prev: Option<&[Option<Digests>]>,
    raws: &RawTables,
    compress: bool,
    (n, start, end, check): DataJob,
) -> Result<Ready> {
    let t = Instant::now();
    let raw = build(&devs[n], &disks[n].1, volumes, start, end)?;
    let unchanged = match (check, prev) {
        (Some(Check::Vol(v, k)), Some(prev)) => prev[v].as_ref().is_some_and(|old| volumes[v].unchanged(old, k)),
        (Some(Check::Raw), _) => {
            let h = xxhash_rust::xxh3::xxh3_64(&raw).max(1);
            raws.cur.lock().unwrap()[n].insert(start, h);
            prev.is_some() && raws.prev[n].get(&start) == Some(&h)
        }
        _ => false,
    };
    let len = raw.len() as u64;
    let packed = match (unchanged, compress) {
        (true, _) => Vec::new(),
        (false, true) => lz4_flex::block::compress(&raw),
        (false, false) => raw,
    };
    Ok(Ready { packed, len, unchanged, read_time: t.elapsed() })
}

fn open_devices(disks: &[(Disk, Vec<Interval>)]) -> Result<Vec<Device>> {
    Ok(disks.iter().map(|(d, _)| Device::open(&d.device())).collect::<std::io::Result<_>>()?)
}

/// Sends the plan's block stream.
#[allow(clippy::too_many_arguments)]
pub fn upload(
    job: &Job,
    logc: &mut Connection,
    disks: &[(Disk, Vec<Interval>)],
    plans: &[Vec<Piece>],
    volumes: &[Vol],
    prev: Option<&[Option<Digests>]>,
    snapshot_time: Duration,
    (compress, encrypt): (bool, bool),
    raws: &RawTables,
) -> Result<()> {
    let sess = job.sess;
    let devs = open_devices(disks)?;
    let jobs = data_jobs(plans);
    let mut up = if encrypt { sess.connect()? } else { sess.connect_plain()? };
    let open = up.call(&request(
        "backup",
        vec![
            ("subaction", Value::str("upload_block")),
            ("is_cache", Value::Int(0)),
            ("running_task_id", Value::Int(job.rt.running_task_id)),
            ("token", Value::str(crate::session::token(&sess.state))),
        ],
    ))?;
    if open.get("success").and_then(Value::as_int) != Some(1) {
        return Err(Error::Protocol(format!("upload_block rejected: {open:?}")));
    }
    parallel_ordered(
        jobs.len(),
        |i| produce(&devs, disks, volumes, prev, raws, compress, jobs[i]),
        |next| send_stream(job, logc, &mut up, disks, plans, snapshot_time, compress, next),
    )
}

/// Offline test: everything a full backup would send, read, reassembled, verified and
/// compressed. Returns (bytes read, compressed bytes).
/// With `image`, the data of each disk is also written to `<image>.<disk>.img` (sparse), as the
/// server would store it.
pub fn bench(disks: &[(Disk, Vec<Interval>)], volumes: &[Vol], limit: Option<u64>, image: Option<&str>) -> Result<(u64, u64)> {
    let plans: Vec<Vec<Piece>> = disks.iter().map(|(d, iv)| plan_full(d, iv, volumes)).collect();
    let devs = open_devices(disks)?;
    let mut jobs = data_jobs(&plans);
    if let Some(limit) = limit {
        let mut total = 0;
        jobs.retain(|j| {
            total += j.2 - j.1;
            total <= limit
        });
    }
    let files: Vec<Option<std::fs::File>> = disks
        .iter()
        .map(|(d, _)| {
            image
                .map(|i| {
                    let f = std::fs::File::create(format!("{i}.{}.img", d.name))?;
                    f.set_len(d.size)?;
                    Ok::<_, std::io::Error>(f)
                })
                .transpose()
        })
        .collect::<std::io::Result<_>>()?;
    let t = Instant::now();
    parallel_ordered(
        jobs.len(),
        |i| produce(&devs, disks, volumes, None, &RawTables::default(), true, jobs[i]),
        |next| {
            let (mut raw, mut packed) = (0u64, 0u64);
            for k in 0..jobs.len() {
                let r = next()?;
                if let Some(f) = &files[jobs[k].0] {
                    let data = lz4_flex::block::decompress(&r.packed, r.len as usize)
                        .map_err(|e| Error::Protocol(format!("lz4: {e}")))?;
                    std::os::unix::fs::FileExt::write_all_at(f, &data, jobs[k].1)?;
                }
                raw += r.len;
                packed += r.packed.len() as u64;
                if k % 4096 == 4095 {
                    let s = t.elapsed().as_secs_f64();
                    eprintln!("  {:.1} GiB in {s:.0}s ({:.0} MiB/s)", raw as f64 / (1u64 << 30) as f64, raw as f64 / MIB as f64 / s);
                }
            }
            Ok((raw, packed))
        },
    )
}

#[derive(Default)]
struct Counters {
    read_bytes: u64,
    read_time: Duration,
    send_bytes: u64,
    send_time: Duration,
    sent_compressed: u64,
}

fn perf(c: &Counters, snapshot: Option<Duration>) -> Value {
    let pair = |size: u64, t: Duration| Value::map([("size", Value::Int(size)), ("time", Value::Int(t.as_nanos() as u64))]);
    let mut m = vec![("read_disk", pair(c.read_bytes, c.read_time)), ("send", pair(c.send_bytes, c.send_time))];
    if let Some(s) = snapshot {
        m.push(("snapshot", pair(0, s)));
    }
    Value::map([("performance", Value::map(m))])
}

#[allow(clippy::too_many_arguments)]
fn send_stream(
    job: &Job,
    logc: &mut Connection,
    up: &mut Connection,
    disks: &[(Disk, Vec<Interval>)],
    plans: &[Vec<Piece>],
    snapshot_time: Duration,
    compress: bool,
    next: &mut dyn FnMut() -> Result<Ready>,
) -> Result<()> {
    let t0 = Instant::now();
    RUNS_SENT.store(0, Ordering::Relaxed);
    let mut ctr = Counters::default();
    let mut first_perf = Some(snapshot_time);
    let mut last_perf = Instant::now();
    let (mut blocks, mut unchanged, mut sent_raw) = (0u64, 0u64, 0u64);
    let total: u64 = plans.iter().flatten().map(|p| if let Piece::Data { start, end, .. } = p { end - start } else { 0 }).sum();
    let mut progress = crate::state::Progress { pid: std::process::id(), phase: "sending".into(), total, ..Default::default() };
    progress.save();
    let (mut last_progress, mut last_done) = (Instant::now(), 0);
    let mut throttle = Throttle::default();
    for ((d, _), plan) in disks.iter().zip(plans) {
        let image = d.image_name();
        let mut pending: Option<Run> = None;
        for piece in plan {
            if crate::control::should_stop() {
                return Err(Error::Protocol("backup cancelled".into()));
            }
            match piece {
                Piece::Log(t, a) => job.log(logc, *t, Some(a)),
                Piece::Run(r) => {
                    let mut o = r.start;
                    while o < r.end {
                        let e = (o + MAX_UNUSED_RUN).min(r.end);
                        push_run(up, &image, &mut pending, Run { start: o, end: e, ..*r })?;
                        o = e;
                    }
                }
                Piece::Data { start, end, cbt, check } => {
                    let ready = next()?;
                    ctr.read_time += ready.read_time;
                    ctr.read_bytes += ready.len;
                    progress.done += ready.len;
                    if last_progress.elapsed() > Duration::from_secs(2) {
                        progress.rate = ((progress.done - last_done) as f64 / last_progress.elapsed().as_secs_f64()) as u64;
                        progress.save();
                        (last_progress, last_done) = (Instant::now(), progress.done);
                    }
                    if ready.unchanged {
                        unchanged += 1;
                        // Taken from the previous version: an area outside the volumes too.
                        let cbt = *cbt || matches!(check, Some(Check::Raw));
                        let r = Run { start: *start, end: *end, used: true, changed: false, cbt };
                        push_run(up, &image, &mut pending, r)?;
                        continue;
                    }
                    if let Some(p) = pending.take() {
                        send_run(up, &image, &p)?;
                    }
                    ctr.sent_compressed += ready.packed.len() as u64;
                    blocks += 1;
                    sent_raw += ready.len;
                    let msg = Value::map([
                        ("buffer", Value::Buffer(ready.packed)),
                        ("changed", Value::Int(1)),
                        ("compressed", Value::Int(compress as u64)),
                        ("image_name", Value::str(image.clone())),
                        ("is_interval_cbt", Value::Int(*cbt as u64)),
                        ("length", Value::Int(ready.len)),
                        ("offset", Value::Int(*start)),
                        ("used", Value::Int(1)),
                    ]);
                    let ts = Instant::now();
                    let packed_len = msg.get("buffer").map_or(0, |b| match b {
                        Value::Buffer(b) => b.len() as u64,
                        _ => 0,
                    });
                    up.send(&msg)?;
                    throttle.sent(packed_len);
                    ctr.send_time += ts.elapsed();
                    ctr.send_bytes += ready.len;
                }
            }
            if let Some(snap) = first_perf.take_if(|_| ctr.send_bytes > 0) {
                up.send(&perf(&ctr, Some(snap)))?;
                ctr = Counters { sent_compressed: ctr.sent_compressed, ..Default::default() };
                last_perf = Instant::now();
            }
            if last_perf.elapsed() > Duration::from_secs(5) {
                up.send(&perf(&ctr, None))?;
                ctr = Counters { sent_compressed: ctr.sent_compressed, ..Default::default() };
                last_perf = Instant::now();
            }
        }
        if let Some(p) = pending.take() {
            send_run(up, &image, &p)?;
        }
    }
    up.send(&perf(&ctr, None))?;
    eprintln!("all pieces sent after {:.1}s ({blocks} with data, {runs} without), waiting for the server", t0.elapsed().as_secs_f64(), runs = RUNS_SENT.load(Ordering::Relaxed));
    crate::state::Progress { phase: "finishing".into(), done: progress.total, ..progress }.save();
    up.send(&Value::map([("close", Value::Int(1))]))?;
    up.set_read_timeout(Some(Duration::from_secs(3600)))?;
    let r = up.recv()?;
    if r.get("close").and_then(Value::as_int) != Some(1) {
        return Err(Error::Protocol(format!("unexpected stream close: {r:?}")));
    }
    eprintln!(
        "stream closed: {blocks} pieces sent ({:.1} MiB, {:.1} MiB compressed), {unchanged} read and unchanged",
        sent_raw as f64 / MIB as f64,
        ctr.sent_compressed as f64 / MIB as f64
    );
    Ok(())
}

/// Bandwidth limit set in the console (`control::bandwidth`, bytes per second): after each
/// message, sleeps as needed to keep the average rate under the limit.
#[derive(Default)]
struct Throttle {
    since: Option<(Instant, u64)>,
    bytes: u64,
}

impl Throttle {
    fn sent(&mut self, n: u64) {
        let limit = crate::control::bandwidth();
        // Restart the measure when the limit changes.
        if self.since.is_none_or(|(_, l)| l != limit) {
            self.since = (limit > 0).then(|| (Instant::now(), limit));
            self.bytes = 0;
        }
        let Some((start, _)) = self.since else { return };
        self.bytes += n;
        let due = Duration::from_secs_f64(self.bytes as f64 / limit as f64);
        if let Some(wait) = due.checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
    }
}

static RUNS_SENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn send_run(up: &mut Connection, image: &str, r: &Run) -> Result<()> {
    RUNS_SENT.fetch_add(1, Ordering::Relaxed);
    up.send(&Value::map([
        ("changed", Value::Int(r.changed as u64)),
        ("changed_length", Value::Int(0)),
        ("image_name", Value::str(image)),
        ("is_interval_cbt", Value::Int(r.cbt as u64)),
        ("length", Value::Int(r.end - r.start)),
        ("offset", Value::Int(r.start)),
        ("used", Value::Int(r.used as u64)),
    ]))
}

/// Queues a piece without data: merges it with the previous one if contiguous with the same flags.
fn push_run(up: &mut Connection, image: &str, pending: &mut Option<Run>, r: Run) -> Result<()> {
    if let Some(p) = pending.as_mut()
        && p.end == r.start
        && (p.used, p.changed, p.cbt) == (r.used, r.changed, r.cbt)
        && r.end - p.start <= MAX_UNUSED_RUN
    {
        p.end = r.end;
        return Ok(());
    }
    if let Some(p) = pending.take() {
        send_run(up, image, &p)?;
    }
    *pending = Some(r);
    Ok(())
}
