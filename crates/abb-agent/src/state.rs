//! Persistent agent state: server, certificate fingerprint, device token.
//! Stored in `/var/lib/abb-agent/state.json`, readable only by root (it holds the token).

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const STATE_DIR: &str = "/var/lib/abb-agent";

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
}

impl LastRun {
    fn path() -> PathBuf {
        Path::new(STATE_DIR).join("last-run.json")
    }

    pub fn load() -> LastRun {
        fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let tmp = Self::path().with_extension("tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self).map_err(std::io::Error::other)?)?;
        fs::rename(tmp, Self::path())
    }
}

fn path() -> PathBuf {
    Path::new(STATE_DIR).join("state.json")
}

impl State {
    pub fn load() -> std::io::Result<State> {
        let text = fs::read_to_string(path())?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    pub fn save(&self) -> std::io::Result<()> {
        fs::create_dir_all(STATE_DIR)?;
        fs::set_permissions(STATE_DIR, fs::Permissions::from_mode(0o700))?;
        let tmp = path().with_extension("tmp");
        let mut f = fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(self).map_err(std::io::Error::other)?.as_bytes())?;
        f.sync_all()?;
        fs::rename(tmp, path())
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
