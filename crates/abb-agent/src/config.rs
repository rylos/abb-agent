//! User configuration: `/etc/abb-agent.toml` (or the file named by `ABB_AGENT_CONFIG`).
//! Every setting is optional; a missing file means all defaults. The server connection is not
//! here: it is agent state, created by `abb-agent connect`.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

pub const DEFAULT_PATH: &str = "/etc/abb-agent.toml";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Disks to back up: kernel names ("nvme0n1"), device paths (/dev/disk/by-id/…) or "system";
    /// empty = all internal disks.
    pub disks: Vec<String>,
    pub schedule: Schedule,
    pub btrfs: Btrfs,
    pub ext: Ext,
    pub performance: Performance,
    pub hooks: Hooks,
    pub agent: Agent,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Agent {
    /// Build of the official agent to announce instead of abb-agent's (4 digits): only when the
    /// NAS refuses this agent as too old, until an abb-agent release announces the new build.
    pub announce_build: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Schedule {
    /// One backup after every boot, as soon as the server is reachable (on top of the schedule
    /// set in the ABB console).
    pub on_boot: bool,
    /// Hours between backups, on top of the console schedule (0 = none).
    pub interval_hours: u64,
    /// Minutes before retrying a failed backup.
    pub retry_minutes: u64,
    /// Lock files to wait for (exclusive flock) before a backup; missing files are ignored.
    pub wait_locks: Vec<String>,
    /// Shell command that must succeed for the agent to start a scheduled backup on its own
    /// (e.g. "on the home network"); checked again every 30 s while a backup is due. Empty =
    /// always. Backups started from the console or by hand do not check it.
    pub only_if: String,
}

impl Default for Schedule {
    fn default() -> Schedule {
        Schedule { on_boot: true, interval_hours: 0, retry_minutes: 60, wait_locks: Vec::new(), only_if: String::new() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Btrfs {
    /// Subvolumes to leave out of the image: paths from the top-level subvolume, `*` and `?`
    /// allowed (e.g. "@games", "btrbk_snapshots/@games.*"). Nested subvolumes go with them.
    pub exclude_subvolumes: Vec<String>,
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hooks {
    /// Shell command run when a backup starts (after waiting for `wait_locks`).
    pub on_start: String,
    /// Shell command run when a backup ends, with ABB_STATUS and ABB_EXIT in the environment.
    pub on_finish: String,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn path() -> String {
    std::env::var("ABB_AGENT_CONFIG").unwrap_or_else(|_| DEFAULT_PATH.into())
}

/// Reads the configuration once; later calls to [`get`] return it.
pub fn load() -> Result<&'static Config, String> {
    let path = path();
    let config = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("{path}: {e}{}", misplaced(&e.to_string())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("{path}: {e}")),
    };
    Ok(CONFIG.get_or_init(|| config))
}

/// For an unknown field that is a setting of a section (a key uncommented without its
/// `[section]` line, or put in another section): where it belongs.
fn misplaced(error: &str) -> String {
    let Some(key) = error.split("unknown field `").nth(1).and_then(|r| r.split('`').next()) else {
        return String::new();
    };
    let Ok(toml::Value::Table(sections)) = toml::Value::try_from(Config::default()) else {
        return String::new();
    };
    sections
        .iter()
        .find(|(_, v)| v.as_table().is_some_and(|t| t.contains_key(key)))
        .map(|(name, _)| format!("`{key}` is a setting of [{name}]: it must come after the `[{name}]` line"))
        .unwrap_or_default()
}

/// The configuration in effect (defaults if [`load`] was never called).
pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_file_parses_and_keys_need_their_section() {
        let shipped = include_str!("../../../packaging/abb-agent.toml");
        toml::from_str::<Config>(shipped).unwrap();
        // Uncommenting a setting under its section line works.
        let edited = shipped.replace("# on_boot = true", "on_boot = false");
        assert!(!toml::from_str::<Config>(&edited).unwrap().schedule.on_boot);
        // Without the section line, the error says where it belongs.
        let e = toml::from_str::<Config>("on_boot = false").unwrap_err().to_string();
        assert!(misplaced(&e).contains("[schedule]"), "{e}");
    }
}
