//! User configuration: `/etc/abb-agent.toml` (or the file named by `ABB_AGENT_CONFIG`).
//! Every setting is optional; a missing file means all defaults. The server connection is not
//! here: it is agent state, created by `abb-agent connect`.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

pub const DEFAULT_PATH: &str = "/etc/abb-agent.toml";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Disks to back up (kernel names, e.g. "nvme0n1"); empty = the disk holding `/`.
    pub disks: Vec<String>,
    pub schedule: Schedule,
    pub ext: Ext,
    pub performance: Performance,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Schedule {
    /// One backup after every boot, as soon as the server is reachable.
    pub on_boot: bool,
    /// Hours between backups (0 = only after boot).
    pub interval_hours: u64,
    /// Minutes before retrying a failed backup.
    pub retry_minutes: u64,
    /// Lock files to wait for (exclusive flock) before a backup; missing files are ignored.
    pub wait_locks: Vec<String>,
}

impl Default for Schedule {
    fn default() -> Schedule {
        Schedule { on_boot: true, interval_hours: 24, retry_minutes: 60, wait_locks: Vec::new() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ext {
    /// Freeze read-write mounted ext filesystems for the whole upload instead of skipping them.
    pub freeze_mounted: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Performance {
    /// Threads that read, verify and compress data (0 = one per CPU, at most 8).
    pub workers: usize,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn path() -> String {
    std::env::var("ABB_AGENT_CONFIG").unwrap_or_else(|_| DEFAULT_PATH.into())
}

/// Reads the configuration once; later calls to [`get`] return it.
pub fn load() -> Result<&'static Config, String> {
    let path = path();
    let config = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("{path}: {e}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("{path}: {e}")),
    };
    Ok(CONFIG.get_or_init(|| config))
}

/// The configuration in effect (defaults if [`load`] was never called).
pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}
