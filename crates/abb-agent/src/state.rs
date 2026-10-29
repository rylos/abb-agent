//! Persistent agent state: server, certificate fingerprint, device token.
//! Stored in `/var/lib/abb-agent/state.json`, readable only by root (it holds the token).

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const STATE_DIR: &str = "/var/lib/abb-agent";

/// Replaces `path` with `data` so that a crash leaves either the old file or the new one: written
/// to a temporary file of its own next to it (created with `mode`), synced, renamed over `path`,
/// and the directory synced.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().ok_or_else(|| std::io::Error::other(format!("{}: not a file", path.display())))?;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{}.{}-{seq}.tmp", name.to_string_lossy(), std::process::id()));
    let res = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(dir)?.sync_all()
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub server: String,
    pub port: u16,
    /// SHA-256 fingerprint of the server certificate, in hex.
    pub fingerprint: String,
    pub ds_id: String,
    pub token: String,
    pub username: String,
    pub device_uuid: String,
    /// Last server event already handled (`long_poll`).
    #[serde(default)]
    pub last_event_id: u64,
}

/// Result of the last backup (separate file: written by whoever runs the backup).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LastRun {
    /// Start of the last attempt (Unix seconds).
    pub start: u64,
    pub end: u64,
    /// "complete", "partial" or "error".
    pub status: String,
    /// Start of the last successful backup (complete or partial).
    pub last_ok_start: u64,
    /// Result of the last backup on the server, and its final status there (the server
    /// verifies each new version after the upload: "success", "partial", "failure", …).
    #[serde(default)]
    pub result_id: u64,
    #[serde(default)]
    pub server_status: String,
}

impl LastRun {
    fn path() -> PathBuf {
        Path::new(STATE_DIR).join("last-run.json")
    }

    pub fn load() -> LastRun {
        fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        write_atomic(&Self::path(), serde_json::to_string_pretty(self).map_err(std::io::Error::other)?.as_bytes(), 0o644)
    }
}

/// Progress of the running backup, for `abb-agent status` (in /run: gone after a reboot).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Progress {
    /// Process running the backup: the file counts only while it is alive.
    pub pid: u32,
    /// "preparing" (snapshots, plan), "sending" or "finishing" (the server checks the version).
    pub phase: String,
    /// Bytes of data processed (sent, or read and found unchanged) and to process.
    pub done: u64,
    pub total: u64,
    /// Bytes per second over the last few seconds.
    pub rate: u64,
}

impl Progress {
    const PATH: &str = "/run/abb-agent/progress.json";

    pub fn load() -> Option<Progress> {
        let p: Progress = serde_json::from_str(&fs::read_to_string(Self::PATH).ok()?).ok()?;
        Path::new("/proc").join(p.pid.to_string()).exists().then_some(p)
    }

    pub fn save(&self) {
        let path = Path::new(Self::PATH);
        let _ = fs::create_dir_all(path.parent().unwrap())
            .and_then(|_| write_atomic(path, serde_json::to_string(self).unwrap_or_default().as_bytes(), 0o644));
    }

    /// Sets the phase of this process's backup.
    pub fn phase(phase: &str, done: u64, total: u64) {
        Progress { pid: std::process::id(), phase: phase.into(), done, total, rate: 0 }.save();
    }

    pub fn clear() {
        let _ = fs::remove_file(Self::PATH);
    }
}

fn path() -> PathBuf {
    Path::new(STATE_DIR).join("state.json")
}

/// Keeps the image name of each disk stable: the kernel may name disks differently from one boot
/// or kernel to the next (nvme0n1 and nvme1n1 swapping), but on the server a disk must keep its
/// image, or every backup after a change would be a full one in a new image. The first name a
/// disk was backed up under is remembered by its stable id (`disk-images.json`). Disks without a
/// stable id keep their kernel name. Ids no longer present (a replaced disk, a restore onto new
/// hardware) give their name up.
pub fn image_names(disks: &mut [crate::disk::Disk]) {
    let file = Path::new(STATE_DIR).join("disk-images.json");
    let mut map: std::collections::BTreeMap<String, String> =
        fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let present: Vec<Option<String>> = disks.iter().map(|d| crate::disk::stable_id(&d.name)).collect();
    let live_ids: Vec<String> = crate::disk::internal_disks()
        .unwrap_or_default()
        .iter()
        .filter_map(|n| crate::disk::stable_id(n))
        .chain(present.iter().flatten().cloned())
        .collect();
    map.retain(|id, _| live_ids.contains(id));
    for (d, id) in disks.iter_mut().zip(&present) {
        let Some(id) = id else { continue };
        let name = match map.get(id) {
            Some(n) => n.clone(),
            None => {
                let mut n = d.name.clone();
                let mut k = 2;
                while map.values().any(|v| *v == n) {
                    n = format!("{}-{k}", d.name);
                    k += 1;
                }
                map.insert(id.clone(), n.clone());
                n
            }
        };
        if name != d.name {
            eprintln!("{}: image {name}.img (the disk's name when first backed up)", d.name);
        }
        d.image = name;
    }
    let _ = fs::create_dir_all(STATE_DIR);
    if let Ok(t) = serde_json::to_string_pretty(&map)
        && let Err(e) = write_atomic(&file, t.as_bytes(), 0o644)
    {
        eprintln!("saving {}: {e}", file.display());
    }
}

impl State {
    pub fn load() -> std::io::Result<State> {
        let text = fs::read_to_string(path())?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    pub fn save(&self) -> std::io::Result<()> {
        // One save at a time in this process: the last one called is the one that stays.
        static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = SAVING.lock().unwrap_or_else(|e| e.into_inner());
        fs::create_dir_all(STATE_DIR)?;
        fs::set_permissions(STATE_DIR, fs::Permissions::from_mode(0o700))?;
        write_atomic(&path(), serde_json::to_string_pretty(self).map_err(std::io::Error::other)?.as_bytes(), 0o600)
    }

    pub fn pin(&self) -> Option<abb_proto::Fingerprint> {
        let bytes: Vec<u8> = self
            .fingerprint
            .split(':')
            .filter_map(|h| u8::from_str_radix(h, 16).ok())
            .collect();
        bytes.try_into().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_atomic_writes_never_mix() {
        let dir = std::env::temp_dir().join(format!("abb-agent-state-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("state.json");
        let writers: Vec<_> = (0..8u8)
            .map(|i| {
                let file = file.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        write_atomic(&file, &vec![b'a' + i; 4096 * (i as usize + 1)], 0o600).unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        let data = fs::read(&file).unwrap();
        assert!(data.iter().all(|&b| b == data[0]) && data.len() == 4096 * (data[0] - b'a' + 1) as usize);
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "temporary files left");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_atomic_replaces_and_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("abb-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.json");
        write_atomic(&f, b"one", 0o600).unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"one");
        assert_eq!(fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        write_atomic(&f, b"two", 0o600).unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("x.json")]);
        // A failed write keeps the old content and removes its temporary file.
        assert!(write_atomic(&dir.join("missing/y.json"), b"z", 0o600).is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
