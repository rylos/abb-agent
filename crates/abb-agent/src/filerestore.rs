//! Restore of files from the ABB portal onto this device ("Restore" rather than "Download").
//!
//! The server queues a running task of type 1 (`file_restore_task`) and sends
//! `update_running_task`. The agent opens one data channel (`restore/get_file`) and receives
//! `{has_more, file}` messages: each file in pieces of at most 32 MiB, in order, with the
//! absolute destination path; owner, mode and times come with the last piece (and with
//! directories). It reports the finished paths (`update_done_ids`), which is all the portal's
//! progress counts, logs each one (`log_report`, 6003…) and ends with `check_complete`.
//! A file is written to a temporary file next to it and renamed into place, holes left sparse.
//! Directories get their times at the end, deepest first, so that the files written inside
//! do not change them.

use crate::host::now;
use crate::session::Session;
use abb_proto::{Connection, Error, Value};
use serde_json::json;
use std::collections::HashSet;
use std::fs::File;
use std::io::ErrorKind;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
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
            let s = Session::new(state);
            if let Err(e) = run(&s, r) {
                eprintln!("file restore {id}: {e}");
            }
            ACTIVE.lock().unwrap().as_mut().map(|a| a.remove(&id));
        });
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
            let logs = std::mem::take(&mut self.logs);
            if let Err(e) = self.sess.call(&mut c, "log_report", vec![("subaction", Value::str("result_detail")), ("logs", Value::Array(logs))]) {
                eprintln!("log_report: {e}");
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
    report.flush()?;
    let mut c = sess.connect()?;
    let reply = sess.call(
        &mut c,
        "restore",
        vec![
            ("subaction", Value::str("check_complete")),
            ("running_task_id", Value::Int(id)),
            ("status", Value::str(status)),
        ],
    )?;
    eprintln!(
        "file restore {id}: {} ({} restored, {} skipped, {} failed)",
        reply.get("status").and_then(Value::as_str).unwrap_or(status),
        report.restored,
        report.skipped,
        report.failed
    );
    Ok(())
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
        writer.handle(file, r.compression, report).map_err(Stop::Failed)?;
        if int(&msg, "has_more") == 0 {
            return Ok(());
        }
    }
}

/// With `conflict_handle: "skip"`: the server lists the paths it is about to send and the agent
/// answers with those that already exist, which then come with `skip`. `{alive}` every 5 s
/// keeps the channel open meanwhile.
fn precheck(ch: &mut Connection, msg: &Value) -> Result<()> {
    let dest = text(msg, "destination");
    let Some(Value::Array(paths)) = msg.get("paths") else { return ch.send(&Value::map([("paths", Value::Array(vec![]))])) };
    let mut exist = Vec::new();
    let mut alive = Instant::now();
    for p in paths {
        let Some(rel) = p.as_str() else { continue };
        if alive.elapsed() > Duration::from_secs(5) {
            ch.send(&Value::map([("alive", Value::Int(1))]))?;
            alive = Instant::now();
        }
        if let Ok(full) = target(&dest, rel)
            && std::fs::symlink_metadata(&full).is_ok()
        {
            exist.push(p.clone());
        }
    }
    ch.send(&Value::map([("paths", Value::Array(exist))]))
}

/// Destination path of a `file`: absolute, no `..`.
fn target(dest: &str, path: &str) -> std::result::Result<PathBuf, String> {
    let p = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        Path::new(if dest.is_empty() { "/" } else { dest }).join(path)
    };
    if p.components().any(|c| matches!(c, Component::ParentDir)) || !p.is_absolute() {
        return Err(format!("unsafe path {path:?}"));
    }
    Ok(p)
}

/// The file being received.
struct Partial {
    id: u64,
    path: PathBuf,
    temp: PathBuf,
    file: File,
    replaced: bool,
}

/// Writes what the server sends.
struct Writer {
    skip_existing: bool,
    current: Option<Partial>,
    /// Paths finished (later pieces of them are ignored).
    finished: HashSet<u64>,
    /// Directories restored, with their metadata, applied at the end.
    dirs: Vec<(PathBuf, Meta)>,
}

#[derive(Clone, Copy, Debug)]
struct Meta {
    uid: u32,
    gid: u32,
    mode: u32,
    atime: i64,
    mtime: i64,
}

impl Meta {
    fn from(f: &Value) -> Option<Meta> {
        f.get("mode")?;
        Some(Meta {
            uid: int(f, "uid") as u32,
            gid: int(f, "gid") as u32,
            mode: int(f, "mode") as u32,
            atime: int(f, "atime") as i64,
            mtime: int(f, "mtime") as i64,
        })
    }

    /// Owner, permissions and times; failures are only logged, as by the official agent.
    fn apply(&self, path: &Path) {
        if let Err(e) = std::os::unix::fs::lchown(path, Some(self.uid), Some(self.gid)) {
            eprintln!("{}: chown: {e}", path.display());
        }
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(self.mode & 0o7777)) {
            eprintln!("{}: chmod: {e}", path.display());
        }
        let times = [
            libc::timespec { tv_sec: self.atime as libc::time_t, tv_nsec: 0 },
            libc::timespec { tv_sec: self.mtime as libc::time_t, tv_nsec: 0 },
        ];
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else { return };
        if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) } != 0 {
            eprintln!("{}: times: {}", path.display(), std::io::Error::last_os_error());
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

impl Writer {
    fn new(r: &Restore) -> Writer {
        Writer { skip_existing: r.skip_existing, current: None, finished: HashSet::new(), dirs: vec![] }
    }

    /// Drops a file received only in part (channel lost or restore stopped).
    fn abandon(&mut self) {
        if let Some(p) = self.current.take() {
            let _ = std::fs::remove_file(&p.temp);
        }
    }

    fn handle(&mut self, f: &Value, compressed: bool, report: &mut Report) -> std::result::Result<(), String> {
        let id = int(f, "id");
        let raw_path = text(f, "path");
        if self.finished.contains(&id) {
            return Ok(());
        }
        let path = match target(&report.r.destination, &raw_path) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("file restore: {e}");
                self.finished.insert(id);
                report.finish(id, DONE_ERROR, LOG_ERROR, &raw_path, json!({}));
                return Ok(());
            }
        };
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

    /// Makes sure the parent directories exist (created if missing: the server may send a file
    /// before, or without, its directory). Something else in the way is an error, or with
    /// "skip" a reason to skip.
    fn parent(&self, path: &Path) -> std::result::Result<bool, (u64, String)> {
        let Some(parent) = path.parent() else { return Ok(true) };
        match std::fs::metadata(parent) {
            Ok(m) if m.is_dir() => Ok(true),
            Ok(_) if self.skip_existing => Ok(false),
            Ok(_) => Err((LOG_PARENT_NOT_DIR, format!("{} is not a directory", parent.display()))),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                std::fs::create_dir_all(parent).map_err(|e| (log_for(&e), format!("creating {}: {e}", parent.display())))?;
                Ok(true)
            }
            Err(e) => Err((log_for(&e), e.to_string())),
        }
    }

    /// A directory: created if missing, an existing directory is kept as it is, anything
    /// else in its place is replaced. With "skip" anything existing is skipped, as by the
    /// official agent. `None`: skipped.
    fn dir(&mut self, path: &Path, meta: Option<Meta>) -> std::result::Result<Option<u64>, (u64, String)> {
        self.abandon();
        if !self.parent(path)? {
            return Ok(None);
        }
        let log = match std::fs::symlink_metadata(path) {
            Ok(_) if self.skip_existing => return Ok(None),
            Ok(m) if m.is_dir() => LOG_RESTORED,
            Ok(_) => {
                std::fs::remove_file(path).map_err(|e| (log_for(&e), e.to_string()))?;
                std::fs::create_dir(path).map_err(|e| (log_for(&e), e.to_string()))?;
                LOG_REPLACED
            }
            Err(_) => {
                std::fs::create_dir(path).map_err(|e| (log_for(&e), e.to_string()))?;
                LOG_RESTORED
            }
        };
        if let Some(m) = meta {
            self.dirs.push((path.to_path_buf(), m));
        }
        Ok(Some(log))
    }

    /// One piece of a file. `Some(log)` once the file is complete (or skipped).
    fn piece(&mut self, id: u64, path: &Path, f: &Value, compressed: bool) -> std::result::Result<Option<u64>, (u64, String)> {
        let offset = int(f, "offset");
        let length = int(f, "length");
        let size = int(f, "size");
        if offset == 0 {
            self.abandon();
            if !self.parent(path)? {
                return Ok(Some(LOG_SKIPPED));
            }
            let existing = std::fs::symlink_metadata(path).ok();
            if existing.is_some() && self.skip_existing {
                return Ok(Some(LOG_SKIPPED));
            }
            let dir = path.parent().unwrap_or(Path::new("/"));
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let temp = dir.join(format!(".{name}.abb-restore-{}", std::process::id()));
            let file = File::options()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temp)
                .map_err(|e| (log_for(&e), format!("creating {}: {e}", temp.display())))?;
            self.current = Some(Partial { id, path: path.to_path_buf(), temp, file, replaced: existing.is_some() });
        }
        let Some(cur) = self.current.as_mut().filter(|c| c.id == id) else {
            return Err((LOG_ERROR, format!("piece at {offset} of a file not being received")));
        };
        let data = match f.get("buffer") {
            Some(Value::Buffer(b)) if compressed || int(f, "compression") != 0 => {
                if b.len() as u64 == length {
                    b.clone()
                } else {
                    lz4_flex::block::decompress(b, length as usize).map_err(|e| (LOG_ERROR, format!("lz4: {e}")))?
                }
            }
            Some(Value::Buffer(b)) => b.clone(),
            _ => vec![],
        };
        if data.len() as u64 != length {
            return Err((LOG_ERROR, format!("piece at {offset}: {} bytes instead of {length}", data.len())));
        }
        // Blocks of zeros are not written: they stay holes, as in a sparse file.
        for (i, block) in data.chunks(HOLE).enumerate() {
            if block.iter().all(|&b| b == 0) {
                continue;
            }
            cur.file
                .write_all_at(block, offset + (i * HOLE) as u64)
                .map_err(|e| (log_for(&e), format!("writing {}: {e}", cur.temp.display())))?;
        }
        if offset + length < size {
            return Ok(None);
        }
        let cur = self.current.take().unwrap();
        let fin = cur.file.set_len(size).and_then(|()| cur.file.sync_all());
        if let Err(e) = fin {
            let _ = std::fs::remove_file(&cur.temp);
            return Err((log_for(&e), format!("writing {}: {e}", cur.temp.display())));
        }
        drop(cur.file);
        if let Some(m) = Meta::from(f) {
            m.apply(&cur.temp);
        }
        // A directory in the way is replaced too (rename does not replace one).
        if let Ok(m) = std::fs::symlink_metadata(&cur.path)
            && m.is_dir()
            && let Err(e) = std::fs::remove_dir_all(&cur.path)
        {
            let _ = std::fs::remove_file(&cur.temp);
            return Err((log_for(&e), format!("removing {}: {e}", cur.path.display())));
        }
        if let Err(e) = std::fs::rename(&cur.temp, &cur.path) {
            let _ = std::fs::remove_file(&cur.temp);
            return Err((log_for(&e), format!("renaming into {}: {e}", cur.path.display())));
        }
        Ok(Some(if cur.replaced { LOG_REPLACED } else { LOG_RESTORED }))
    }

    /// Directory metadata, deepest first, once their content is written.
    fn apply_dir_metadata(&mut self) {
        let mut dirs = std::mem::take(&mut self.dirs);
        dirs.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        for (p, m) in dirs {
            if std::fs::symlink_metadata(&p).is_ok_and(|x| x.is_dir() && x.ino() != 0) {
                m.apply(&p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_paths() {
        assert_eq!(target("/home/u", "/home/u/a/b").unwrap(), PathBuf::from("/home/u/a/b"));
        assert_eq!(target("/", "etc/x").unwrap(), PathBuf::from("/etc/x"));
        assert!(target("/tmp", "/tmp/../etc/shadow").is_err());
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

    fn writer(skip: bool) -> Writer {
        Writer { skip_existing: skip, current: None, finished: HashSet::new(), dirs: vec![] }
    }

    #[test]
    fn writes_files_sparse_and_in_place() {
        let dir = std::env::temp_dir().join(format!("abb-filerestore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Parent missing: created.
        let p = dir.join("a/b/f");
        let mut w = writer(false);
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
        assert_eq!(writer(true).piece(3, &p, &file(3, &p, 0, b"x", 1), false).unwrap(), Some(LOG_SKIPPED));
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        // A directory where a file goes is replaced; an empty file is written.
        let d = dir.join("a/b");
        assert_eq!(w.piece(4, &d, &file(4, &d, 0, b"", 0), false).unwrap(), Some(LOG_REPLACED));
        assert!(std::fs::metadata(&d).unwrap().is_file());
        // Directory over an existing one: kept, its times applied at the end.
        let dd = dir.join("dd");
        let meta = Meta { uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() }, mode: 0o40750, atime: 5, mtime: 5 };
        assert_eq!(w.dir(&dd, Some(meta)).unwrap(), Some(LOG_RESTORED));
        assert_eq!(w.piece(5, &dd.join("x"), &file(5, &dd.join("x"), 0, b"1", 1), false).unwrap(), Some(LOG_RESTORED));
        w.apply_dir_metadata();
        assert_eq!(std::fs::metadata(&dd).unwrap().mtime(), 5);
        // No temporary files left.
        let left: Vec<_> = walk(&dir).into_iter().filter(|p| p.to_string_lossy().contains("abb-restore")).collect();
        assert!(left.is_empty(), "{left:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn walk(d: &Path) -> Vec<PathBuf> {
        let mut out = vec![];
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            }
            out.push(p);
        }
        out
    }
}
