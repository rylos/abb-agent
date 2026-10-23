//! When to back up. The schedule and backup window set in the ABB console are followed as
//! the official agent does (in server time; a start missed while the machine was off runs once
//! when it comes back). The configuration adds what the console cannot express for Linux: a
//! backup after every boot, a fixed interval, and retries after a failure. Backups started from
//! the console run whenever the server asks.

use crate::backup::{EVENT_STARTUP, TRIGGER_EVENT, TRIGGER_SCHEDULE};
use crate::config::{self, Schedule as LocalSchedule};
use crate::control;
use crate::host::now;
use crate::session::Session;
use crate::state::{LastRun, STATE_DIR};
use crate::task::TaskInfo;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// A backup the agent decides to start: (trigger_type, trigger_event).
pub type LocalTrigger = (u64, Option<u64>);

/// System boot time (Unix seconds), from /proc/stat.
fn boot_time() -> u64 {
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok())))
        .unwrap_or(0)
}

/// Backup due from the local settings: when, and with which trigger.
pub fn local_due(sc: &LocalSchedule, last: &LastRun) -> Option<(u64, LocalTrigger)> {
    let failed = last.start > 0 && (last.status == "error" || last.status == "running");
    // A backup cancelled by the user counts as done for the boot and interval triggers: they
    // fire again only at the next boot or after the next interval, not right away.
    let done = if last.status == "cancel" { last.last_ok_start.max(last.start) } else { last.last_ok_start };
    let mut due: Vec<(u64, LocalTrigger)> = Vec::new();
    if failed && sc.retry_minutes > 0 {
        due.push((last.start + sc.retry_minutes * 60, (TRIGGER_SCHEDULE, None)));
    }
    if sc.on_boot && done < boot_time() && !failed {
        due.push((0, (TRIGGER_EVENT, Some(EVENT_STARTUP))));
    }
    if sc.interval_hours > 0 && done > 0 {
        due.push((done + sc.interval_hours * 3600, (TRIGGER_SCHEDULE, None)));
    }
    due.into_iter().min_by_key(|d| d.0)
}

/// Last time the console schedule was checked (UTC seconds).
#[derive(Default, Serialize, Deserialize)]
struct Checked {
    checked: u64,
}

fn checked_path() -> std::path::PathBuf {
    Path::new(STATE_DIR).join("schedule.json")
}

fn load_checked() -> u64 {
    std::fs::read_to_string(checked_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Checked>(&t).ok())
        .map_or(0, |c| c.checked)
}

fn save_checked(t: u64) {
    if let Ok(text) = serde_json::to_string(&Checked { checked: t }) {
        let _ = std::fs::write(checked_path(), text);
    }
}

/// Next start from the console schedule (UTC seconds), counted from the last check.
pub fn console_due(task: &TaskInfo) -> Option<u64> {
    let checked = match load_checked() {
        0 => now(),
        c => c,
    };
    let off = control::server_offset();
    let next = task.schedule.next_after(checked as i64 + off)?;
    Some(to_utc(next, off) as u64)
}

/// Server wall-clock seconds to UTC. The server gives only its current offset; when it matches
/// the local one (same time zone, clocks within a few minutes) the local rules also cover a
/// daylight saving change before `t` (the console's 03:00 stays 03:00 after the switch).
fn to_utc(t: i64, off: i64) -> i64 {
    let skew = off - crate::host::utc_offset(now() as i64);
    let utc = if skew.abs() < 900 { t - crate::host::utc_offset(t - off) - skew } else { t - off };
    utc.max(0)
}

pub fn refresh_timezone(sess: &Session) {
    match sess.connect().and_then(|mut c| sess.server_offset(&mut c)) {
        Ok(off) => control::set_server_offset(off),
        Err(e) => control::net_error("get_timezone", &e),
    }
}

pub fn run(sess: Session, worker: Sender<LocalTrigger>) {
    if load_checked() == 0 {
        save_checked(now());
    }
    let mut task: Option<TaskInfo> = None;
    let mut fetched: Option<Instant> = None;
    let mut tz: Option<Instant> = None;
    let mut waiting = false;
    loop {
        if tz.is_none_or(|t| t.elapsed() > Duration::from_secs(3600)) {
            refresh_timezone(&sess);
            tz = Some(Instant::now());
        }
        if control::take_tasks_changed() || fetched.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) {
            match sess.connect().and_then(|mut c| sess.tasks(&mut c)) {
                Ok(list) => {
                    task = list.first().and_then(TaskInfo::parse);
                    fetched = Some(Instant::now());
                }
                Err(e) => control::net_error("get_task", &e),
            }
        }
        // A backup started by hand (`abb-agent backup`) holds the lock in another process.
        if !control::busy() && !crate::backup::in_progress() {
            let t = now();
            let console = task.as_ref().and_then(console_due).filter(|&d| d <= t);
            let local = local_due(&config::get().schedule, &LastRun::load()).filter(|d| d.0 <= t);
            let due = console.is_some() || local.is_some();
            // `only_if` (e.g. "on the home network"): the backup stays due until it holds.
            let held = due && !crate::hooks::allowed();
            if held && !waiting {
                eprintln!("backup due, waiting for schedule.only_if to hold");
            }
            waiting = held;
            if held {
                // Checked again at the next round.
            } else if console.is_some() {
                eprintln!("scheduled backup (console schedule)");
                save_checked(t);
                let _ = worker.send((TRIGGER_SCHEDULE, None));
            } else if let Some((_, trigger)) = local {
                eprintln!("scheduled backup ({})", if trigger.1.is_some() { "after boot" } else { "local schedule" });
                let _ = worker.send(trigger);
            }
        }
        std::thread::sleep(Duration::from_secs(30));
    }
}
