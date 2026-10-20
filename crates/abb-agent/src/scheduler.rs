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
    let mut due: Vec<(u64, LocalTrigger)> = Vec::new();
    if failed && sc.retry_minutes > 0 {
        due.push((last.start + sc.retry_minutes * 60, (TRIGGER_SCHEDULE, None)));
    }
    if sc.on_boot && last.last_ok_start < boot_time() && !failed {
        due.push((0, (TRIGGER_EVENT, Some(EVENT_STARTUP))));
    }
    if sc.interval_hours > 0 && last.last_ok_start > 0 {
        due.push((last.last_ok_start + sc.interval_hours * 3600, (TRIGGER_SCHEDULE, None)));
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
    task.schedule.next_after(checked as i64 + off).map(|t| (t - off).max(0) as u64)
}

pub fn refresh_timezone(sess: &Session) {
    match sess.connect().and_then(|mut c| sess.server_offset(&mut c)) {
        Ok(off) => control::set_server_offset(off),
        Err(e) => eprintln!("get_timezone: {e}"),
    }
}

pub fn run(sess: Session, worker: Sender<LocalTrigger>) {
    if load_checked() == 0 {
        save_checked(now());
    }
    let mut task: Option<TaskInfo> = None;
    let mut fetched: Option<Instant> = None;
    let mut tz: Option<Instant> = None;
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
                Err(e) => eprintln!("get_task: {e}"),
            }
        }
        if !control::busy() {
            let t = now();
            let console = task.as_ref().and_then(console_due).filter(|&d| d <= t);
            let local = local_due(&config::get().schedule, &LastRun::load()).filter(|d| d.0 <= t);
            if console.is_some() {
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
