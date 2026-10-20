//! Local backup schedule: one at every boot and then every `interval_hours`, retrying
//! after `retry_minutes` on failure. The schedule set on the server is ignored; backups
//! started from the server ("Back up now") still work and count as the last backup.

use crate::backup::{self, TRIGGER_SCHEDULE, Trigger};
use crate::host::now;
use crate::session::Session;
use crate::config::{self, Schedule};
use crate::state::LastRun;
use std::time::Duration;

/// System boot time (Unix seconds), from /proc/stat.
fn boot_time() -> u64 {
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok())))
        .unwrap_or(0)
}

/// When the next backup is due (`None` = never).
pub fn next_due(sc: &Schedule, last: &LastRun) -> Option<u64> {
    if last.start == 0 {
        return Some(0);
    }
    if last.status == "error" || last.status == "running" {
        return Some(last.start + sc.retry_minutes * 60);
    }
    if sc.on_boot && last.last_ok_start < boot_time() {
        return Some(0);
    }
    (sc.interval_hours > 0).then(|| last.last_ok_start + sc.interval_hours * 3600)
}

pub fn run(sess: Session) {
    loop {
        let last = LastRun::load();
        if next_due(&config::get().schedule, &last).is_some_and(|t| t <= now()) {
            eprintln!("scheduled backup");
            if let Err(e) = backup::run(&sess, Trigger::Local { trigger_type: TRIGGER_SCHEDULE }) {
                eprintln!("scheduled backup: {e}");
                // Failed before starting (e.g. server unreachable): retry later.
                let mut l = LastRun::load();
                if l.start == last.start {
                    l.start = now();
                    l.status = "error".into();
                    let _ = l.save();
                }
            }
            // A backup requested by the server in the meantime.
            crate::run_pending(&sess);
        }
        std::thread::sleep(Duration::from_secs(60));
    }
}
