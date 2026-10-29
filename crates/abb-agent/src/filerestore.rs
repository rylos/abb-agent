//! Restore of files from the ABB portal onto this device ("Restore" rather than "Download").
//!
//! The server queues a running task of type 1 (`file_restore_task`) and sends
//! `update_running_task`. The agent opens one data channel (`restore/get_file`) and receives
//! `{has_more, file}` messages: each file in pieces of at most 32 MiB, in order, with the
//! absolute destination path; owner, mode and times come with the last piece (and with
//! directories). It reports the finished paths (`update_done_ids`), which is all the portal's
//! progress counts, logs each one (`log_report`, 6003…) and ends with `check_complete`.
//! A file is written to an unnamed temporary file in its directory and renamed into place,
//! holes left sparse. Directories get their times at the end, deepest first, so that the files
//! written inside do not change them.
//!
//! The daemon runs as root and the destination may hold directories other users can write:
//! every path is opened one component at a time from "/" without following symlinks (see
//! [`Writer::walk`]), and everything is done relative to directory and file descriptors.

use crate::host::now;
use crate::session::Session;
use abb_proto::{Connection, Error, Value};
use serde_json::json;
use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::ffi::{CStr, CString, OsString};
use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Error>;

/// Restores being run, by running task id.
static ACTIVE: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

/// Running task status on the server.
const RUNNING: u64 = 1;
const CANCELLED: u64 = 3;
const FAILED: u64 = 5;

/// `file_type` in a `file` message.
const TYPE_FILE: u64 = 0;
const TYPE_DIR: u64 = 1;
const TYPE_SERVER_ERROR: u64 = 2;
const TYPE_OPEN_FAILED: u64 = 9998;
const TYPE_UNSUPPORTED: u64 = 9999;

/// `error` reported for a path in `update_done_ids`.
const DONE_OK: u64 = 0;
const DONE_ERROR: u64 = 1;
const DONE_UNSUPPORTED: u64 = 2;

/// `log_report` codes for each path.
const LOG_RESTORED: u64 = 6003;
const LOG_SKIPPED: u64 = 6004;
const LOG_REPLACED: u64 = 6005;
const LOG_ERROR: u64 = 6006;
const LOG_UNSUPPORTED: u64 = 6011;
const LOG_NO_PARENT: u64 = 6015;
const LOG_PARENT_NOT_DIR: u64 = 6016;
const LOG_DENIED: u64 = 6017;
const LOG_SERVER_ERROR: u64 = 6018;
const LOG_NO_SPACE: u64 = 6020;
const LOG_OPEN_FAILED: u64 = 6021;

/// How often finished paths are reported and the running task is checked for a cancel.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// Largest piece of a file the server sends (uncompressed).
const MAX_PIECE: u64 = 32 << 20;

/// A file restore queued for this device.
#[derive(Clone, Debug)]
pub struct Restore {
    pub running_task_id: u64,
    pub task_id: u64,
    pub task_uuid: String,
    pub result_id: u64,
    pub status: u64,
    pub destination: String,
    /// `conflict_handle: "skip"`: keep what already exists.
    pub skip_existing: bool,
    pub encryption: bool,
    pub compression: bool,
}

fn int(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_int().or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))).unwrap_or(0)
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

/// File restores in a `get_running_task` reply (elements with `type:1`).
pub fn parse(reply: &Value) -> Vec<Restore> {
    let Some(Value::Array(list)) = reply.get("running_task_list") else { return vec![] };
    list.iter()
        .filter(|e| int(e, "type") == 1)
        .filter_map(|e| {
            let f = e.get("file_restore_task")?;
            Some(Restore {
                running_task_id: e.get("id").and_then(Value::as_int).unwrap_or(int(f, "running_task_id")),
                task_id: int(e, "task_id"),
                task_uuid: text(f, "task_uuid"),
                result_id: int(e, "result_id"),
                status: int(e, "status"),
                destination: text(f, "destination"),
                skip_existing: text(f, "conflict_handle") == "skip",
                encryption: int(f, "encryption") != 0,
                compression: int(f, "compression") != 0,
            })
        })
        .collect()
}

fn pending(sess: &Session) -> Result<Vec<Restore>> {
    let mut c = sess.connect()?;
    Ok(parse(&sess.call(&mut c, "get_running_task", vec![])?))
}

/// After `update_running_task` and at startup: starts a thread for each new file restore.
pub fn check(sess: &Session) {
    let list = match pending(sess) {
        Ok(l) => l,
        Err(e) => return eprintln!("get_running_task: {e}"),
    };
    for r in list {
        if r.status == CANCELLED || r.status == FAILED {
            continue;
        }
        {
            let mut active = ACTIVE.lock().unwrap();
            if !active.get_or_insert_with(HashSet::new).insert(r.running_task_id) {
                continue;
            }
        }
        let state = sess.state.clone();
        std::thread::spawn(move || {
            let id = r.running_task_id;
            // Even after a panic: otherwise the restore could never start again.
            let _active = Active(id);
            let s = Session::new(state);
            if let Err(e) = run(&s, r) {
                eprintln!("file restore {id}: {e}");
            }
        });
    }
}

/// A restore in [`ACTIVE`] while this lives.
struct Active(u64);

impl Drop for Active {
    fn drop(&mut self) {
        if let Some(a) = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            a.remove(&self.0);
        }
    }
}

/// Paths finished and not yet reported, with their log entries.
struct Report<'a> {
    sess: &'a Session,
    r: &'a Restore,
    done: Vec<Value>,
    logs: Vec<Value>,
    last: Instant,
    restored: u64,
    skipped: u64,
    failed: u64,
}

impl Report<'_> {
    fn finish(&mut self, id: u64, error: u64, log_type: u64, path: &str, extra: serde_json::Value) {
        match log_type {
            LOG_SKIPPED => self.skipped += 1,
            LOG_RESTORED | LOG_REPLACED => self.restored += 1,
            _ => self.failed += 1,
        }
        self.done.push(Value::map([("error", Value::Int(error)), ("id", Value::Int(id))]));
        let mut params = json!({
            "path": path,
            "result_id": self.r.result_id,
            "running_task_id": self.r.running_task_id,
            "task_id": self.r.task_id,
            "task_uuid": self.r.task_uuid,
        });
        if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        self.logs.push(Value::map([
            ("log_time", Value::Int(now())),
            ("log_type", Value::Int(log_type)),
            ("other_params", Value::str(serde_json::to_string_pretty(&params).unwrap() + "\n")),
            ("running_task_id", Value::Int(self.r.running_task_id)),
        ]));
    }

    /// Reports finished paths; returns the running task's status on the server (0 if gone).
    fn flush(&mut self) -> Result<u64> {
        self.last = Instant::now();
        let mut c = self.sess.connect()?;
        if !self.logs.is_empty() {
            // Kept until sent: tried again with the next report.
            let logs = Value::Array(self.logs.clone());
            match self.sess.call(&mut c, "log_report", vec![("subaction", Value::str("result_detail")), ("logs", logs)]) {
                Ok(_) => self.logs.clear(),
                Err(e) => eprintln!("log_report: {e}"),
            }
        }
        if !self.done.is_empty() {
            self.sess.call(
                &mut c,
                "restore",
                vec![
                    ("subaction", Value::str("update_done_ids")),
                    ("running_task_id", Value::Int(self.r.running_task_id)),
                    ("done_ids", Value::Array(self.done.clone())),
                ],
            )?;
            self.done.clear();
        }
        let list = parse(&self.sess.call(&mut c, "get_running_task", vec![])?);
        Ok(list.iter().find(|x| x.running_task_id == self.r.running_task_id).map_or(0, |x| x.status))
    }
}

/// Why a restore stops before the end.
enum Stop {
    /// Cancelled in the portal, or the server ended the task.
    Cancelled,
    Failed(String),
    /// Channel lost: open it again, the server sends the paths not reported yet.
    Disconnected(Error),
}

pub fn run(sess: &Session, r: Restore) -> Result<()> {
    let id = r.running_task_id;
    // The official agent starts only once the server lists the task as running.
    let waited = Instant::now();
    let mut r = r;
    while r.status != RUNNING {
        if r.status == CANCELLED || r.status == FAILED || waited.elapsed() > Duration::from_secs(1800) {
            eprintln!("file restore {id}: not started (status {} on the server)", r.status);
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(5));
        match pending(sess)?.into_iter().find(|x| x.running_task_id == id) {
            Some(x) => r = x,
            None => return Ok(()),
        }
    }
    eprintln!(
        "file restore {id}: to {} ({})",
        if r.destination.is_empty() { "the original location" } else { &r.destination },
        if r.skip_existing { "keeping existing files" } else { "replacing existing files" }
    );
    let mut report = Report { sess, r: &r, done: vec![], logs: vec![], last: Instant::now(), restored: 0, skipped: 0, failed: 0 };
    let mut writer = Writer::new(&r);
    let mut attempts = 0;
    let outcome = loop {
        match receive(sess, &r, &mut writer, &mut report) {
            Ok(()) => break Ok(()),
            Err(Stop::Disconnected(e)) if attempts < 5 => {
                attempts += 1;
                eprintln!("file restore {id}: {e}; reconnecting");
                writer.abandon();
                // Finished paths reported first, or the server sends them again.
                match report.flush() {
                    Ok(RUNNING) => {}
                    Ok(_) => break Err(Stop::Cancelled),
                    Err(e) => eprintln!("file restore {id}: reporting progress: {e}"),
                }
                std::thread::sleep(Duration::from_secs(10 * attempts));
            }
            Err(Stop::Disconnected(e)) => break Err(Stop::Failed(e.to_string())),
            Err(other) => break Err(other),
        }
    };
    writer.abandon();
    writer.apply_dir_metadata();
    let status = match outcome {
        Ok(()) => "complete",
        Err(Stop::Cancelled) => {
            let _ = report.flush();
            eprintln!("file restore {id}: cancelled ({} restored, {} skipped, {} failed)", report.restored, report.skipped, report.failed);
            return Ok(());
        }
        Err(Stop::Failed(e)) => {
            eprintln!("file restore {id}: {e}");
            "error"
        }
        Err(Stop::Disconnected(_)) => unreachable!(),
    };
    // Without check_complete the task would stay running on the server: tried a few times.
    let mut attempts = 0;
    let reply = loop {
        match complete(sess, &mut report, status) {
            Ok(reply) => break reply,
            Err(e) if attempts < 5 => {
                attempts += 1;
                eprintln!("file restore {id}: ending the task: {e}; trying again");
                std::thread::sleep(Duration::from_secs(5 << attempts));
            }
            Err(e) => return Err(e),
        }
    };
    eprintln!(
        "file restore {id}: {} ({} restored, {} skipped, {} failed)",
        reply.get("status").and_then(Value::as_str).unwrap_or(status),
        report.restored,
        report.skipped,
        report.failed
    );
    Ok(())
}

/// Last report and `check_complete` with the outcome.
fn complete(sess: &Session, report: &mut Report, status: &str) -> Result<Value> {
    report.flush()?;
    let mut c = sess.connect()?;
    sess.call(
        &mut c,
        "restore",
        vec![
            ("subaction", Value::str("check_complete")),
            ("running_task_id", Value::Int(report.r.running_task_id)),
            ("status", Value::str(status)),
        ],
    )
}

/// One `get_file` channel, until the server says there is nothing more.
fn receive(sess: &Session, r: &Restore, writer: &mut Writer, report: &mut Report) -> std::result::Result<(), Stop> {
    let mut ch = if r.encryption { sess.connect() } else { sess.connect_plain() }.map_err(Stop::Disconnected)?;
    let req = crate::session::authed(
        &sess.state,
        "restore",
        vec![("subaction", Value::str("get_file")), ("running_task_id", Value::Int(r.running_task_id))],
    );
    ch.send(&req).map_err(Stop::Disconnected)?;
    loop {
        if report.last.elapsed() > REPORT_EVERY {
            match report.flush() {
                Ok(RUNNING) => {}
                Ok(_) => return Err(Stop::Cancelled),
                Err(e) => eprintln!("file restore {}: reporting progress: {e}", r.running_task_id),
            }
        }
        let msg = ch.recv().map_err(Stop::Disconnected)?;
        abb_proto::transport::check_error(&msg).map_err(|e| Stop::Failed(e.to_string()))?;
        if int(&msg, "precheck") != 0 {
            precheck(&mut ch, &msg).map_err(Stop::Disconnected)?;
            continue;
        }
        let Some(file) = msg.get("file") else {
            if int(&msg, "has_more") == 0 {
                return Ok(());
            }
            continue;
        };
        writer.handle(file, report).map_err(Stop::Failed)?;
        if int(&msg, "has_more") == 0 {
            return Ok(());
        }
    }
}

/// With `conflict_handle: "skip"`: the server lists the paths it is about to send (relative to
/// `destination`) and the agent answers with those that already exist, which then come with
/// `skip`. `{alive}` every 5 s keeps the channel open meanwhile.
fn precheck(ch: &mut Connection, msg: &Value) -> Result<()> {
    let dest = text(msg, "destination");
    let Some(Value::Array(paths)) = msg.get("paths") else { return ch.send(&Value::map([("paths", Value::Array(vec![]))])) };
    let root = destination(&dest);
    let mut exist = Vec::new();
    let mut alive = Instant::now();
    for p in paths {
        let Some(rel) = p.as_str() else { continue };
        if alive.elapsed() > Duration::from_secs(5) {
            ch.send(&Value::map([("alive", Value::Int(1))]))?;
            alive = Instant::now();
        }
        // Only a hint for the server: what gets written is checked again then.
        if let Ok(full) = target(&root, &format!("{}/{}", dest.trim_end_matches('/'), rel.trim_start_matches('/')))
            && std::fs::symlink_metadata(&full).is_ok()
        {
            exist.push(p.clone());
        }
    }
    ch.send(&Value::map([("paths", Value::Array(exist))]))
}

/// The destination folder: "/" when the task has none.
fn destination(dest: &str) -> PathBuf {
    PathBuf::from(if dest.is_empty() { "/" } else { dest })
}

/// Where a `file` goes: an absolute path (a relative one is taken from `dest`) with no `..`,
/// strictly inside the destination folder. The server sends `destination` + the path relative
/// to it, for the original location too (`destination` is then the folder of the selection).
fn target(dest: &Path, path: &str) -> std::result::Result<PathBuf, String> {
    let clean = |p: &Path| p.is_absolute() && !p.components().any(|c| matches!(c, Component::ParentDir));
    let p = dest.join(path);
    if !clean(&p) {
        return Err(format!("unsafe path {path:?}"));
    }
    if !clean(dest) || !p.starts_with(dest) || p.components().count() <= dest.components().count() {
        return Err(format!("{path:?} is outside the destination {}", dest.display()));
    }
    Ok(p)
}

/// The file being received.
struct Partial {
    id: u64,
    path: PathBuf,
    /// The directory it goes into, and its name there.
    dir: OwnedFd,
    name: CString,
    /// Name of the temporary file; `None` while it has none (`O_TMPFILE`).
    temp: Option<CString>,
    file: File,
    replaced: bool,
}

impl Partial {
    /// Removes the temporary file.
    fn discard(self) {
        if let Some(t) = &self.temp {
            let _ = at::unlink(self.dir.as_fd(), t);
        }
    }
}

/// Writes what the server sends.
struct Writer {
    /// Destination folder. Its own components may be symlinks only if set up by root, see
    /// [`Writer::walk`].
    dest: PathBuf,
    skip_existing: bool,
    /// Temporary files without a name (`O_TMPFILE`) tried first.
    tmpfile: bool,
    current: Option<Partial>,
    /// Paths finished (later pieces of them are ignored).
    finished: HashSet<u64>,
    /// Directories restored, with their metadata, applied at the end.
    dirs: Vec<(PathBuf, Meta)>,
}

/// Owner, permissions and times, each only if the server sent it.
#[derive(Clone, Copy, Debug)]
struct Meta {
    uid: Option<u32>,
    gid: Option<u32>,
    mode: u32,
    atime: Option<i64>,
    mtime: Option<i64>,
}

impl Meta {
    fn from(f: &Value) -> Option<Meta> {
        f.get("mode")?;
        let opt = |k| f.get(k).map(|_| int(f, k));
        Some(Meta {
            uid: opt("uid").map(|x| x as u32),
            gid: opt("gid").map(|x| x as u32),
            mode: int(f, "mode") as u32,
            atime: opt("atime").map(|x| x as i64),
            mtime: opt("mtime").map(|x| x as i64),
        })
    }

    /// Applied through an open descriptor: owner first (a chown clears setuid and setgid), then
    /// permissions and times. Failures are only logged, as by the official agent.
    fn apply(&self, fd: BorrowedFd, path: &Path) {
        if (self.uid.is_some() || self.gid.is_some())
            && let Err(e) = std::os::unix::fs::fchown(fd, self.uid, self.gid)
        {
            eprintln!("{}: chown: {e}", path.display());
        }
        if let Err(e) = at::chmod(fd, self.mode & 0o7777) {
            eprintln!("{}: chmod: {e}", path.display());
        }
        let time = |t: Option<i64>| match t {
            Some(t) => libc::timespec { tv_sec: t as libc::time_t, tv_nsec: 0 },
            None => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
        };
        if (self.atime.is_some() || self.mtime.is_some())
            && let Err(e) = at::set_times(fd, [time(self.atime), time(self.mtime)])
        {
            eprintln!("{}: times: {e}", path.display());
        }
    }
}

/// Log code for a local error.
fn log_for(e: &std::io::Error) -> u64 {
    match e.raw_os_error() {
        Some(libc::EACCES | libc::EPERM | libc::EROFS) => LOG_DENIED,
        Some(libc::ENOSPC | libc::EDQUOT) => LOG_NO_SPACE,
        _ => LOG_ERROR,
    }
}

/// Granularity at which zero blocks are left as holes.
const HOLE: usize = 4096;

/// Writes `data` at `offset` with `write`, leaving blocks of zeros out (holes, as in a sparse
/// file); consecutive blocks with data go in one write.
fn write_sparse(data: &[u8], offset: u64, mut write: impl FnMut(&[u8], u64) -> std::io::Result<()>) -> std::io::Result<()> {
    let mut run = None;
    for (i, block) in data.chunks(HOLE).enumerate() {
        let at = i * HOLE;
        match (block.iter().all(|&b| b == 0), run) {
            (false, None) => run = Some(at),
            (true, Some(start)) => {
                write(&data[start..at], offset + start as u64)?;
                run = None;
            }
            _ => {}
        }
    }
    match run {
        Some(start) => write(&data[start..], offset + start as u64),
        None => Ok(()),
    }
}

/// Whether a piece is in LZ4: each piece says so, the task's setting counts only if it does not.
fn compressed(f: &Value, task: bool) -> bool {
    match f.get("compression") {
        Some(_) => int(f, "compression") != 0,
        None => task,
    }
}

/// A new temporary name: `.abb-restore-` and 16 random hex digits.
fn temp_name() -> CString {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut b = [0u8; 8];
    let x = if unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) } == b.len() as isize {
        u64::from_ne_bytes(b)
    } else {
        // Names are created exclusively: a guessed one is only skipped.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        t.as_nanos() as u64 ^ (std::process::id() as u64) << 40 ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15)
    };
    CString::new(format!(".abb-restore-{x:016x}")).unwrap()
}

/// `f` with new temporary names until one is free.
fn with_temp_name<T>(mut f: impl FnMut(&CStr) -> std::io::Result<T>) -> std::io::Result<(T, CString)> {
    let mut tries = 0;
    loop {
        let n = temp_name();
        match f(&n) {
            Ok(x) => return Ok((x, n)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists && tries < 16 => tries += 1,
            Err(e) => return Err(e),
        }
    }
}

impl Writer {
    fn new(r: &Restore) -> Writer {
        Writer {
            dest: destination(&r.destination),
            skip_existing: r.skip_existing,
            tmpfile: true,
            current: None,
            finished: HashSet::new(),
            dirs: vec![],
        }
    }

    /// Drops a file received only in part (channel lost or restore stopped).
    fn abandon(&mut self) {
        if let Some(p) = self.current.take() {
            p.discard();
        }
    }

    fn handle(&mut self, f: &Value, report: &mut Report) -> std::result::Result<(), String> {
        let id = int(f, "id");
        let raw_path = text(f, "path");
        if self.finished.contains(&id) {
            return Ok(());
        }
        let path = match target(&self.dest, &raw_path) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("file restore: {e}");
                self.finished.insert(id);
                report.finish(id, DONE_ERROR, LOG_ERROR, &raw_path, json!({}));
                return Ok(());
            }
        };
        let compressed = compressed(f, report.r.compression);
        let shown = path.display().to_string();
        let mut done = |w: &mut Writer, error: u64, log: u64, extra: serde_json::Value| {
            w.finished.insert(id);
            report.finish(id, error, log, &shown, extra);
        };
        if int(f, "skip") != 0 {
            self.abandon();
            done(self, DONE_OK, LOG_SKIPPED, json!({}));
            return Ok(());
        }
        match int(f, "file_type") {
            TYPE_DIR => {
                let (error, log) = match self.dir(&path, Meta::from(f)) {
                    Ok(Some(log)) => (DONE_OK, log),
                    Ok(None) => (DONE_OK, LOG_SKIPPED),
                    Err((log, e)) => {
                        eprintln!("{shown}: {e}");
                        (DONE_ERROR, log)
                    }
                };
                done(self, error, log, json!({}));
            }
            TYPE_FILE => match self.piece(id, &path, f, compressed) {
                Ok(None) => {}
                Ok(Some(log)) => done(self, DONE_OK, log, json!({})),
                Err((log, e)) => {
                    eprintln!("{shown}: {e}");
                    self.abandon();
                    let extra = match log {
                        LOG_NO_PARENT | LOG_PARENT_NOT_DIR => {
                            json!({"parent_path": path.parent().map(|p| p.display().to_string()).unwrap_or_default()})
                        }
                        _ => json!({}),
                    };
                    done(self, DONE_ERROR, log, extra);
                }
            },
            TYPE_SERVER_ERROR => {
                eprintln!("{shown}: the server could not read it (error {})", int(f, "error_code"));
                done(self, DONE_ERROR, LOG_SERVER_ERROR, json!({"error_code": int(f, "error_code").to_string()}));
            }
            TYPE_OPEN_FAILED => done(self, DONE_UNSUPPORTED, LOG_OPEN_FAILED, json!({})),
            TYPE_UNSUPPORTED => {
                eprintln!("{shown}: not restored, the server sends only files and directories (symlink, fifo, device or socket)");
                done(self, DONE_UNSUPPORTED, LOG_UNSUPPORTED, json!({}));
            }
            t => {
                eprintln!("{shown}: unknown file_type {t}");
                done(self, DONE_UNSUPPORTED, LOG_UNSUPPORTED, json!({}));
            }
        }
        Ok(())
    }

    /// Opens the directory `path` from "/", one component at a time, never following a symlink
    /// a user could have planted: a symlink in the destination folder's own path is followed
    /// only if it belongs to root (the user running the agent) in a directory only root can
    /// write, like `/home -> var/home`; any other is an error, or with "skip" a reason to skip.
    /// Below the destination folder, with `create`, a symlink where a directory goes is replaced
    /// by the directory and missing directories are created (the server may send a file before,
    /// or without, its directory); anything else in the way is an error. Without `create`
    /// nothing is changed and anything but a directory is an error. `None`: skipped.
    fn walk(&self, path: &Path, create: bool) -> std::result::Result<Option<OwnedFd>, (u64, String)> {
        let fixed = self.dest.components().count();
        let mut todo: VecDeque<(OsString, bool)> = path
            .components()
            .enumerate()
            .filter_map(|(i, c)| match c {
                Component::Normal(n) => Some((n.to_owned(), i < fixed)),
                _ => None,
            })
            .collect();
        let err = |here: &Path, e: std::io::Error| (log_for(&e), format!("{}: {e}", here.display()));
        let mut stack = vec![at::root().map_err(|e| err(Path::new("/"), e))?];
        let mut shown = PathBuf::from("/");
        let mut links = 0;
        while let Some((n, fixed)) = todo.pop_front() {
            if n == ".." {
                // Only from the target of a symlink.
                if stack.len() > 1 {
                    stack.pop();
                    shown.pop();
                }
                continue;
            }
            let here = shown.join(&n);
            let cur = stack.last().unwrap().as_fd();
            let name = at::cstr(&n).map_err(|e| err(&here, e))?;
            let st = match at::lstat(cur, &name) {
                Ok(st) => Some(st),
                Err(e) if e.kind() == ErrorKind::NotFound => None,
                Err(e) => return Err(err(&here, e)),
            };
            let not_dir = || (LOG_PARENT_NOT_DIR, format!("{} is not a directory", here.display()));
            match st {
                Some(st) if at::is_dir(&st) => {}
                Some(st) if fixed && at::is_link(&st) && at::trusted(cur, &st) => {
                    links += 1;
                    if links > 40 {
                        return Err((LOG_ERROR, format!("{}: too many symlinks", here.display())));
                    }
                    let to = PathBuf::from(OsString::from_vec(at::readlink(cur, &name).map_err(|e| err(&here, e))?));
                    if to.is_absolute() {
                        stack.truncate(1);
                        shown = PathBuf::from("/");
                    }
                    for c in to.components().rev() {
                        match c {
                            Component::Normal(x) => todo.push_front((x.to_owned(), true)),
                            Component::ParentDir => todo.push_front(("..".into(), true)),
                            _ => {}
                        }
                    }
                    continue;
                }
                None if !create => return Err((LOG_NO_PARENT, format!("{} does not exist", here.display()))),
                Some(_) if !create => return Err(not_dir()),
                Some(_) if self.skip_existing => return Ok(None),
                Some(st) if !fixed && at::is_link(&st) => {
                    at::unlink(cur, &name).map_err(|e| err(&here, e))?;
                    at::mkdir(cur, &name).map_err(|e| err(&here, e))?;
                }
                Some(_) => return Err(not_dir()),
                None => match at::mkdir(cur, &name) {
                    Err(e) if e.kind() != ErrorKind::AlreadyExists => return Err(err(&here, e)),
                    _ => {}
                },
            }
            let d = at::open_dir(cur, &name).map_err(|e| match e.raw_os_error() {
                Some(libc::ELOOP | libc::ENOTDIR) => not_dir(),
                _ => err(&here, e),
            })?;
            stack.push(d);
            shown = here;
        }
        Ok(stack.pop())
    }

    /// The directory `path` goes into, opened (see [`Writer::walk`]), and its name there.
    fn parent(&self, path: &Path) -> std::result::Result<Option<(OwnedFd, CString)>, (u64, String)> {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err((LOG_ERROR, format!("{}: no name", path.display())));
        };
        let name = at::cstr(name).map_err(|e| (LOG_ERROR, format!("{}: {e}", path.display())))?;
        Ok(self.walk(dir, true)?.map(|d| (d, name)))
    }

    /// A directory: created if missing, an existing directory is kept as it is, anything
    /// else in its place is replaced. With "skip" anything existing is skipped, as by the
    /// official agent. `None`: skipped.
    fn dir(&mut self, path: &Path, meta: Option<Meta>) -> std::result::Result<Option<u64>, (u64, String)> {
        self.abandon();
        let Some((d, name)) = self.parent(path)? else { return Ok(None) };
        let (d, err) = (d.as_fd(), |e: std::io::Error| (log_for(&e), e.to_string()));
        let log = match at::lstat(d, &name) {
            Ok(_) if self.skip_existing => return Ok(None),
            Ok(st) if at::is_dir(&st) => LOG_RESTORED,
            Ok(_) => {
                at::unlink(d, &name).map_err(err)?;
                at::mkdir(d, &name).map_err(err)?;
                LOG_REPLACED
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                at::mkdir(d, &name).map_err(err)?;
                LOG_RESTORED
            }
            Err(e) => return Err(err(e)),
        };
        if let Some(m) = meta {
            self.dirs.push((path.to_path_buf(), m));
        }
        Ok(Some(log))
    }

    /// A new temporary file in `d`: without a name if the filesystem can, else with a random
    /// one, created exclusively (never through a symlink or over something existing).
    fn create_temp(&self, d: BorrowedFd) -> std::io::Result<(File, Option<CString>)> {
        if self.tmpfile {
            match at::tmpfile(d) {
                Ok(f) => return Ok((f, None)),
                Err(e) if matches!(e.raw_os_error(), Some(libc::EOPNOTSUPP | libc::EISDIR | libc::EINVAL)) => {}
                Err(e) => return Err(e),
            }
        }
        let (f, n) = with_temp_name(|n| at::create(d, n))?;
        Ok((f, Some(n)))
    }

    /// One piece of a file. `Some(log)` once the file is complete (or skipped).
    fn piece(&mut self, id: u64, path: &Path, f: &Value, compressed: bool) -> std::result::Result<Option<u64>, (u64, String)> {
        let offset = int(f, "offset");
        let length = int(f, "length");
        let size = int(f, "size");
        if length > MAX_PIECE || offset.checked_add(length).is_none_or(|end| end > size) {
            return Err((LOG_ERROR, format!("piece of {length} bytes at {offset} in a file of {size}")));
        }
        if offset == 0 {
            self.abandon();
            let Some((dir, name)) = self.parent(path)? else { return Ok(Some(LOG_SKIPPED)) };
            let replaced = match at::lstat(dir.as_fd(), &name) {
                Ok(_) => true,
                Err(e) if e.kind() == ErrorKind::NotFound => false,
                Err(e) => return Err((log_for(&e), e.to_string())),
            };
            if replaced && self.skip_existing {
                return Ok(Some(LOG_SKIPPED));
            }
            let (file, temp) = self
                .create_temp(dir.as_fd())
                .map_err(|e| (log_for(&e), format!("creating a temporary file next to it: {e}")))?;
            self.current = Some(Partial { id, path: path.to_path_buf(), dir, name, temp, file, replaced });
        }
        let Some(cur) = self.current.as_mut().filter(|c| c.id == id) else {
            return Err((LOG_ERROR, format!("piece at {offset} of a file not being received")));
        };
        let data = match f.get("buffer") {
            Some(Value::Buffer(b)) if compressed && length > 0 => {
                Cow::Owned(lz4_flex::block::decompress(b, length as usize).map_err(|e| (LOG_ERROR, format!("lz4: {e}")))?)
            }
            Some(Value::Buffer(b)) => Cow::Borrowed(b.as_slice()),
            _ => Cow::Borrowed(&[][..]),
        };
        if data.len() as u64 != length {
            return Err((LOG_ERROR, format!("piece at {offset}: {} bytes instead of {length}", data.len())));
        }
        write_sparse(&data, offset, |b, at| cur.file.write_all_at(b, at)).map_err(|e| (log_for(&e), format!("writing: {e}")))?;
        if offset + length < size {
            return Ok(None);
        }
        let cur = self.current.take().unwrap();
        self.place(cur, size, Meta::from(f))
    }

    /// The last piece written: the file gets its size, metadata and name.
    fn place(&self, mut cur: Partial, size: u64, meta: Option<Meta>) -> std::result::Result<Option<u64>, (u64, String)> {
        let fail = |cur: Partial, what: &str, e: std::io::Error| {
            cur.discard();
            Err((log_for(&e), format!("{what}: {e}")))
        };
        if let Err(e) = cur.file.set_len(size).and_then(|()| cur.file.sync_all()) {
            return fail(cur, "writing", e);
        }
        match meta {
            Some(m) => m.apply(cur.file.as_fd(), &cur.path),
            None => {
                let _ = at::chmod(cur.file.as_fd(), 0o644);
            }
        }
        // A name first: a file without one cannot replace another.
        if cur.temp.is_none() {
            match with_temp_name(|n| at::link(&cur.file, cur.dir.as_fd(), n)) {
                Ok(((), n)) => cur.temp = Some(n),
                Err(e) => return fail(cur, "naming the temporary file", e),
            }
        }
        let (d, temp) = (cur.dir.as_fd(), cur.temp.as_deref().unwrap());
        // A directory in the way is replaced too (rename does not replace one).
        if at::lstat(d, &cur.name).is_ok_and(|st| at::is_dir(&st))
            && let Err(e) = at::remove_tree(d, &cur.name)
        {
            return fail(cur, "removing the directory in the way", e);
        }
        if let Err(e) = at::rename(d, temp, &cur.name) {
            return fail(cur, "renaming into place", e);
        }
        Ok(Some(if cur.replaced { LOG_REPLACED } else { LOG_RESTORED }))
    }

    /// Directory metadata, deepest first, once their content is written.
    fn apply_dir_metadata(&mut self) {
        let mut dirs = std::mem::take(&mut self.dirs);
        dirs.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        for (p, m) in dirs {
            match self.walk(&p, false) {
                Ok(Some(d)) => m.apply(d.as_fd(), &p),
                Ok(None) => {}
                Err((_, e)) => eprintln!("{}: metadata not applied: {e}", p.display()),
            }
        }
    }
}

/// Calls relative to a directory descriptor; none follows a symlink in the last component.
mod at {
    use std::ffi::{CStr, CString, OsStr};
    use std::fs::File;
    use std::io::{Error, ErrorKind, Result};
    use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    fn cvt(r: libc::c_int) -> Result<libc::c_int> {
        if r < 0 { Err(Error::last_os_error()) } else { Ok(r) }
    }

    pub fn cstr(n: &OsStr) -> Result<CString> {
        CString::new(n.as_bytes()).map_err(|_| Error::new(ErrorKind::InvalidInput, "NUL in a name"))
    }

    fn open(d: libc::c_int, n: &CStr, flags: libc::c_int) -> Result<OwnedFd> {
        let fd = cvt(unsafe { libc::openat(d, n.as_ptr(), flags | libc::O_CLOEXEC, 0o600 as libc::c_uint) })?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn root() -> Result<OwnedFd> {
        open(libc::AT_FDCWD, c"/", libc::O_RDONLY | libc::O_DIRECTORY)
    }

    /// The directory `n`; a symlink there is an error (`ELOOP`).
    pub fn open_dir(d: BorrowedFd, n: &CStr) -> Result<OwnedFd> {
        open(d.as_raw_fd(), n, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)
    }

    /// A temporary file in `d` without a name.
    pub fn tmpfile(d: BorrowedFd) -> Result<File> {
        open(d.as_raw_fd(), c".", libc::O_TMPFILE | libc::O_WRONLY).map(File::from)
    }

    /// A new file `n`: an error if anything, a symlink too, is already there.
    pub fn create(d: BorrowedFd, n: &CStr) -> Result<File> {
        open(d.as_raw_fd(), n, libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_WRONLY).map(File::from)
    }

    /// Gives the name `n` to a file opened with [`tmpfile`].
    pub fn link(f: &File, d: BorrowedFd, n: &CStr) -> Result<()> {
        let r = unsafe { libc::linkat(f.as_raw_fd(), c"".as_ptr(), d.as_raw_fd(), n.as_ptr(), libc::AT_EMPTY_PATH) };
        match cvt(r) {
            // AT_EMPTY_PATH needs CAP_DAC_READ_SEARCH; the /proc link works for anyone.
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::EPERM)) => {
                let p = CString::new(format!("/proc/self/fd/{}", f.as_raw_fd())).unwrap();
                let r = unsafe { libc::linkat(libc::AT_FDCWD, p.as_ptr(), d.as_raw_fd(), n.as_ptr(), libc::AT_SYMLINK_FOLLOW) };
                cvt(r).map(drop)
            }
            r => r.map(drop),
        }
    }

    pub fn lstat(d: BorrowedFd, n: &CStr) -> Result<libc::stat> {
        let mut st = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::fstatat(d.as_raw_fd(), n.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
        Ok(st)
    }

    fn fstat(d: BorrowedFd) -> Result<libc::stat> {
        let mut st = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::fstat(d.as_raw_fd(), &mut st) })?;
        Ok(st)
    }

    pub fn is_dir(st: &libc::stat) -> bool {
        st.st_mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn is_link(st: &libc::stat) -> bool {
        st.st_mode & libc::S_IFMT == libc::S_IFLNK
    }

    /// A symlink `st` in `d` that only the user running the agent (root) could have made.
    pub fn trusted(d: BorrowedFd, st: &libc::stat) -> bool {
        let me = unsafe { libc::geteuid() };
        st.st_uid == me && fstat(d).is_ok_and(|p| p.st_uid == me && p.st_mode & 0o022 == 0)
    }

    pub fn readlink(d: BorrowedFd, n: &CStr) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        let len = unsafe { libc::readlinkat(d.as_raw_fd(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if len < 0 {
            return Err(Error::last_os_error());
        }
        if len as usize == buf.len() {
            return Err(Error::from_raw_os_error(libc::ENAMETOOLONG));
        }
        buf.truncate(len as usize);
        Ok(buf)
    }

    pub fn mkdir(d: BorrowedFd, n: &CStr) -> Result<()> {
        cvt(unsafe { libc::mkdirat(d.as_raw_fd(), n.as_ptr(), 0o777) }).map(drop)
    }

    /// Removes `n`, not a directory (a symlink itself, never its target).
    pub fn unlink(d: BorrowedFd, n: &CStr) -> Result<()> {
        cvt(unsafe { libc::unlinkat(d.as_raw_fd(), n.as_ptr(), 0) }).map(drop)
    }

    pub fn rename(d: BorrowedFd, from: &CStr, to: &CStr) -> Result<()> {
        cvt(unsafe { libc::renameat(d.as_raw_fd(), from.as_ptr(), d.as_raw_fd(), to.as_ptr()) }).map(drop)
    }

    pub fn chmod(f: BorrowedFd, mode: u32) -> Result<()> {
        cvt(unsafe { libc::fchmod(f.as_raw_fd(), mode as libc::mode_t) }).map(drop)
    }

    pub fn set_times(f: BorrowedFd, times: [libc::timespec; 2]) -> Result<()> {
        cvt(unsafe { libc::futimens(f.as_raw_fd(), times.as_ptr()) }).map(drop)
    }

    /// Removes `n` and, if it is a directory, all it holds; symlinks are removed, not followed.
    pub fn remove_tree(d: BorrowedFd, n: &CStr) -> Result<()> {
        if !is_dir(&lstat(d, n)?) {
            return unlink(d, n);
        }
        let sub = open_dir(d, n)?;
        for e in entries(&sub)? {
            remove_tree(std::os::fd::AsFd::as_fd(&sub), &e)?;
        }
        drop(sub);
        cvt(unsafe { libc::unlinkat(d.as_raw_fd(), n.as_ptr(), libc::AT_REMOVEDIR) }).map(drop)
    }

    /// Names in the directory `d`, without `.` and `..`.
    fn entries(d: &OwnedFd) -> Result<Vec<CString>> {
        let fd = cvt(unsafe { libc::fcntl(d.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })?;
        let dir = unsafe { libc::fdopendir(fd) };
        if dir.is_null() {
            let e = Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        let mut out = vec![];
        let res = loop {
            unsafe { *libc::__errno_location() = 0 };
            let ent = unsafe { libc::readdir(dir) };
            if ent.is_null() {
                let e = Error::last_os_error();
                break if e.raw_os_error() == Some(0) { Ok(()) } else { Err(e) };
            }
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
            if name != c"." && name != c".." {
                out.push(name.to_owned());
            }
        };
        unsafe { libc::closedir(dir) };
        res.map(|()| out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    #[test]
    fn destination_paths() {
        let t = |d: &str, p: &str| target(Path::new(d), p);
        assert_eq!(t("/home/u", "/home/u/a/b").unwrap(), PathBuf::from("/home/u/a/b"));
        assert_eq!(t("/", "etc/x").unwrap(), PathBuf::from("/etc/x"));
        assert_eq!(t("/", "/etc/x").unwrap(), PathBuf::from("/etc/x"));
        assert_eq!(t("/home/u/", "a").unwrap(), PathBuf::from("/home/u/a"));
        assert!(t("/tmp", "/tmp/../etc/shadow").is_err());
        // Absolute paths outside the destination, or the destination itself.
        assert!(t("/home/u", "/etc/shadow").is_err());
        assert!(t("/home/u", "/home/user2/x").is_err());
        assert!(t("/home/u", "/home/u").is_err());
        assert!(t("/home/../etc", "/etc/x").is_err());
        assert_eq!(destination(""), PathBuf::from("/"));
    }

    /// A fresh directory for a test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abb-filerestore-{name}-{}", std::process::id()));
        if dir.exists() {
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn file(id: u64, path: &Path, offset: u64, data: &[u8], size: u64) -> Value {
        let mut m = vec![
            ("id", Value::Int(id)),
            ("path", Value::str(path.to_str().unwrap())),
            ("file_type", Value::Int(TYPE_FILE)),
            ("offset", Value::Int(offset)),
            ("length", Value::Int(data.len() as u64)),
            ("size", Value::Int(size)),
            ("buffer", Value::Buffer(data.to_vec())),
        ];
        if offset + data.len() as u64 == size {
            m.extend([("mode", Value::Int(0o100640)), ("mtime", Value::Int(1_000_000_000)), ("atime", Value::Int(1_000_000_000))]);
            let me = unsafe { (libc::getuid(), libc::getgid()) };
            m.extend([("uid", Value::Int(me.0 as u64)), ("gid", Value::Int(me.1 as u64))]);
        }
        Value::map(m)
    }

    fn writer(dest: &Path, skip: bool) -> Writer {
        Writer { dest: dest.to_path_buf(), skip_existing: skip, tmpfile: true, current: None, finished: HashSet::new(), dirs: vec![] }
    }

    fn meta(mtime: i64) -> Meta {
        Meta { uid: None, gid: None, mode: 0o40750, atime: Some(mtime), mtime: Some(mtime) }
    }

    #[test]
    fn writes_files_sparse_and_in_place() {
        for tmpfile in [true, false] {
            let dir = scratch(if tmpfile { "write-tmpfile" } else { "write-named" });
            // Parent missing: created.
            let p = dir.join("a/b/f");
            let mut w = writer(&dir, false);
            w.tmpfile = tmpfile;
            let mut first = vec![7u8; 10];
            first.extend(vec![0u8; 64 * HOLE]);
            let size = first.len() as u64 + 3;
            assert_eq!(w.piece(1, &p, &file(1, &p, 0, &first, size), false).unwrap(), None);
            assert_eq!(w.piece(1, &p, &file(1, &p, first.len() as u64, b"end", size), false).unwrap(), Some(LOG_RESTORED));
            let m = std::fs::metadata(&p).unwrap();
            assert_eq!(m.len(), size);
            assert!(m.blocks() * 512 < 64 * 1024, "holes kept: {} blocks", m.blocks());
            assert_eq!(m.mode() & 0o7777, 0o640);
            assert_eq!(m.mtime(), 1_000_000_000);
            let data = std::fs::read(&p).unwrap();
            assert_eq!(&data[..10], &[7u8; 10]);
            assert_eq!(&data[first.len()..first.len() + 3], b"end");
            // Existing file: replaced, or kept with "skip".
            assert_eq!(w.piece(2, &p, &file(2, &p, 0, b"new", 3), false).unwrap(), Some(LOG_REPLACED));
            assert_eq!(std::fs::read(&p).unwrap(), b"new");
            assert_eq!(writer(&dir, true).piece(3, &p, &file(3, &p, 0, b"x", 1), false).unwrap(), Some(LOG_SKIPPED));
            assert_eq!(std::fs::read(&p).unwrap(), b"new");
            // A directory where a file goes is replaced; an empty file is written.
            let d = dir.join("a/b");
            assert_eq!(w.piece(4, &d, &file(4, &d, 0, b"", 0), false).unwrap(), Some(LOG_REPLACED));
            assert!(std::fs::metadata(&d).unwrap().is_file());
            // Directory over an existing one: kept, its times applied at the end.
            let dd = dir.join("dd");
            assert_eq!(w.dir(&dd, Some(meta(5))).unwrap(), Some(LOG_RESTORED));
            assert_eq!(w.piece(5, &dd.join("x"), &file(5, &dd.join("x"), 0, b"1", 1), false).unwrap(), Some(LOG_RESTORED));
            w.apply_dir_metadata();
            assert_eq!(std::fs::metadata(&dd).unwrap().mtime(), 5);
            // A file received in part and abandoned leaves nothing behind.
            let q = dir.join("q");
            assert_eq!(w.piece(6, &q, &file(6, &q, 0, b"12", 4), false).unwrap(), None);
            w.abandon();
            assert!(!q.exists());
            // No temporary files left.
            let left: Vec<_> = walk(&dir).into_iter().filter(|p| p.to_string_lossy().contains("abb-restore")).collect();
            assert!(left.is_empty(), "{left:?}");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn planted_temp_symlink_not_followed() {
        let dir = scratch("planted");
        let dest = dir.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let victim = dir.join("shadow");
        std::fs::write(&victim, b"secret").unwrap();
        // Where the temporary file used to go, predictably.
        symlink(&victim, dest.join(format!(".f.abb-restore-{}", std::process::id()))).unwrap();
        for tmpfile in [true, false] {
            let p = dest.join("f");
            let mut w = writer(&dest, false);
            w.tmpfile = tmpfile;
            assert!(w.piece(1, &p, &file(1, &p, 0, b"restored", 8), false).unwrap().is_some());
            assert_eq!(std::fs::read(&p).unwrap(), b"restored");
            assert_eq!(std::fs::read(&victim).unwrap(), b"secret");
            assert_eq!(std::fs::metadata(&victim).unwrap().mode() & 0o7777, 0o644);
        }
        // A temporary name taken by a symlink is never opened through it.
        let d = File::open(&dest).unwrap();
        let n = c"planted";
        symlink(&victim, dest.join("planted")).unwrap();
        assert_eq!(at::create(d.as_fd(), n).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&victim).unwrap(), b"secret");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_replaced_by_symlink() {
        let dir = scratch("dirlink");
        let dest = dir.join("dest");
        let out = dir.join("out");
        std::fs::create_dir(&dest).unwrap();
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("keep"), b"k").unwrap();
        // The directory, restored, then replaced by a symlink before its files arrive.
        let mut w = writer(&dest, false);
        let a = dest.join("a");
        assert_eq!(w.dir(&a, Some(meta(5))).unwrap(), Some(LOG_RESTORED));
        std::fs::remove_dir(&a).unwrap();
        symlink(&out, &a).unwrap();
        // With "skip": skipped, nothing written.
        let x = a.join("x");
        assert_eq!(writer(&dest, true).piece(1, &x, &file(1, &x, 0, b"x", 1), false).unwrap(), Some(LOG_SKIPPED));
        assert_eq!(writer(&dest, true).dir(&a.join("sub"), None).unwrap(), None);
        // Directory metadata not applied through it.
        let before = std::fs::metadata(&out).unwrap().mtime();
        w.apply_dir_metadata();
        assert_eq!(std::fs::metadata(&out).unwrap().mtime(), before);
        assert_ne!(before, 5);
        // Replaced: the symlink goes, the directory is made again.
        assert_eq!(w.piece(2, &x, &file(2, &x, 0, b"x", 1), false).unwrap(), Some(LOG_RESTORED));
        assert!(std::fs::symlink_metadata(&a).unwrap().is_dir());
        assert_eq!(std::fs::read(a.join("x")).unwrap(), b"x");
        // Deeper: a missing directory below a symlink.
        std::fs::remove_dir_all(&a).unwrap();
        symlink(&out, &a).unwrap();
        let y = a.join("b/c/y");
        assert_eq!(w.piece(3, &y, &file(3, &y, 0, b"y", 1), false).unwrap(), Some(LOG_RESTORED));
        assert_eq!(std::fs::read(dest.join("a/b/c/y")).unwrap(), b"y");
        // Nothing outside the destination.
        let mut seen: Vec<_> = walk(&out);
        seen.sort();
        assert_eq!(seen, [out.join("keep")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn destination_through_symlinks() {
        let dir = scratch("destlink");
        let real = dir.join("real");
        std::fs::create_dir(&real).unwrap();
        // Made by the user running the agent in a directory only it can write: followed.
        let dest = dir.join("dest");
        symlink("real", &dest).unwrap();
        let p = dest.join("f");
        assert_eq!(writer(&dest, false).piece(1, &p, &file(1, &p, 0, b"f", 1), false).unwrap(), Some(LOG_RESTORED));
        assert_eq!(std::fs::read(real.join("f")).unwrap(), b"f");
        // In a directory others can write: never followed, nor replaced.
        let open = dir.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let dest = open.join("dest");
        symlink(&real, &dest).unwrap();
        let p = dest.join("g");
        let (log, _) = writer(&dest, false).piece(2, &p, &file(2, &p, 0, b"g", 1), false).unwrap_err();
        assert_eq!(log, LOG_PARENT_NOT_DIR);
        assert_eq!(writer(&dest, true).piece(3, &p, &file(3, &p, 0, b"g", 1), false).unwrap(), Some(LOG_SKIPPED));
        assert!(std::fs::symlink_metadata(&dest).unwrap().file_type().is_symlink());
        assert!(!real.join("g").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_in_the_way_removed_without_following() {
        let dir = scratch("tree");
        let dest = dir.join("dest");
        let out = dir.join("out");
        std::fs::create_dir_all(dest.join("d/e")).unwrap();
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("keep"), b"k").unwrap();
        std::fs::write(dest.join("d/e/f"), b"f").unwrap();
        symlink(&out, dest.join("d/link")).unwrap();
        symlink(&out, dest.join("d/e/link")).unwrap();
        let d = dest.join("d");
        assert_eq!(writer(&dest, false).piece(1, &d, &file(1, &d, 0, b"file", 4), false).unwrap(), Some(LOG_REPLACED));
        assert_eq!(std::fs::read(&d).unwrap(), b"file");
        assert_eq!(std::fs::read(out.join("keep")).unwrap(), b"k");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sparse_writes_coalesced() {
        let mut data = vec![1u8; 3 * HOLE];
        data.extend(vec![0u8; 2 * HOLE]);
        data.extend(vec![2u8; HOLE + 10]);
        let mut writes = vec![];
        write_sparse(&data, 100, |b, at| {
            writes.push((at, b.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(writes, [(100, 3 * HOLE), (100 + 5 * HOLE as u64, HOLE + 10)]);
        writes.clear();
        write_sparse(&[0u8; 3 * HOLE], 0, |b, at| {
            writes.push((at, b.len()));
            Ok(())
        })
        .unwrap();
        assert!(writes.is_empty());
    }

    #[test]
    fn compressed_pieces() {
        let dir = scratch("lz4");
        let mut w = writer(&dir, false);
        // A compressed piece exactly as long as its data still has to be decompressed.
        // 4 literals, a match of 4 bytes 4 back, 1 literal: 9 bytes for 9.
        let packed = [0x40, b'a', b'b', b'c', b'd', 4, 0, 0x10, b'e'];
        let (data, len) = (b"abcdabcde", packed.len());
        let p = dir.join("a");
        let mut f = file(1, &p, 0, &packed, len as u64);
        let Value::Map(m) = &mut f else { unreachable!() };
        m.push(("compression".into(), Value::Int(1)));
        w.handle_piece(&f, false).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), data);
        assert_eq!(len, data.len());
        // The piece's flag wins over the task's.
        let p = dir.join("b");
        let mut f = file(2, &p, 0, b"raw", 3);
        let Value::Map(m) = &mut f else { unreachable!() };
        m.push(("compression".into(), Value::Int(0)));
        w.handle_piece(&f, true).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"raw");
        // Without one, the task's.
        let p = dir.join("c");
        let packed = lz4_flex::block::compress(b"abcabcabcabcabcabc");
        let mut f = file(3, &p, 0, &packed, 18);
        let Value::Map(m) = &mut f else { unreachable!() };
        m.retain(|(k, _)| k != "length");
        m.push(("length".into(), Value::Int(18)));
        w.handle_piece(&f, true).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"abcabcabcabcabcabc");
        // More than the largest piece: refused before anything is allocated.
        let p = dir.join("d");
        let mut f = file(4, &p, 0, b"x", MAX_PIECE + 1);
        let Value::Map(m) = &mut f else { unreachable!() };
        m.retain(|(k, _)| k != "length");
        m.push(("length".into(), Value::Int(MAX_PIECE + 1)));
        assert_eq!(w.piece(4, &p, &f, true).unwrap_err().0, LOG_ERROR);
        assert!(!p.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metadata_only_as_sent() {
        let m = Meta::from(&Value::map([("mode", Value::Int(0o100644)), ("mtime", Value::Int(7))])).unwrap();
        assert_eq!((m.uid, m.gid, m.atime, m.mtime), (None, None, None, Some(7)));
        assert!(Meta::from(&Value::map([("uid", Value::Int(0))])).is_none());
    }

    impl Writer {
        /// A piece through `handle`'s choice of compression.
        fn handle_piece(&mut self, f: &Value, task_compression: bool) -> std::result::Result<Option<u64>, (u64, String)> {
            let path = PathBuf::from(text(f, "path"));
            self.piece(int(f, "id"), &path, f, compressed(f, task_compression))
        }
    }

    fn walk(d: &Path) -> Vec<PathBuf> {
        let mut out = vec![];
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() && !p.is_symlink() {
                out.extend(walk(&p));
            }
            out.push(p);
        }
        out
    }
}
