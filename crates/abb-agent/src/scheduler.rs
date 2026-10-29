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

/// Backup due from the local settings: when, and with which trigger. `since`: when the agent
/// first ran its scheduler, from which the interval counts until a backup has been attempted.
pub fn local_due(sc: &LocalSchedule, last: &LastRun, since: u64) -> Option<(u64, LocalTrigger)> {
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
    // From the last attempt, failed ones included: a failure (or a version the server rejected)
    // without retries must not stop the interval backups.
    let attempted = last.last_ok_start.max(last.start);
    let from = if attempted > 0 { attempted } else { since };
    if sc.interval_hours > 0 && from > 0 {
        due.push((from + sc.interval_hours * 3600, (TRIGGER_SCHEDULE, None)));
    }
    due.into_iter().min_by_key(|d| d.0)
}

/// What the scheduler keeps between runs (`schedule.json`).
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    /// Last time the console schedule was checked (UTC seconds).
    checked: u64,
    /// Server UTC offset less the local one, clock difference included (seconds), from the last
    /// `get_timezone` that answered.
    skew: Option<i64>,
    /// First start of the scheduler (UTC seconds).
    started: u64,
}

fn saved_path() -> std::path::PathBuf {
    Path::new(STATE_DIR).join("schedule.json")
}

fn load() -> Saved {
    std::fs::read_to_string(saved_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Written only by the scheduler thread of the service.
fn save(s: &Saved) {
    let (path, tmp) = (saved_path(), saved_path().with_extension("tmp"));
    if let Ok(text) = serde_json::to_string(s) {
        let _ = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path));
    }
}

/// The server's offset from local time saved by the service, for a process that has not asked.
pub fn saved_skew() -> Option<i64> {
    load().skew
}

/// When the service first ran its scheduler (0 = never).
pub fn started() -> u64 {
    load().started
}

/// Local UTC offset at a Unix time: [`crate::host::utc_offset`], or fixed rules in tests.
type LocalOffset = fn(i64) -> i64;

/// Server offset less the local one at `at`: what stays the same when both switch to or from
/// daylight saving time together.
fn skew_of(off: i64, at: i64, local: LocalOffset) -> i64 {
    off - local(at)
}

/// The server's wall clock. The server gives only its current offset (`skew`: less the local
/// one, clock difference included). When it matches the local one (same time zone, clocks
/// within a few minutes) the local rules also cover its daylight saving changes: the console's
/// 03:00 stays 03:00 after a switch. Otherwise its offset now is taken as fixed (and fetched
/// again when the local one changes).
struct Clock {
    skew: i64,
    local: LocalOffset,
    now: i64,
}

impl Clock {
    fn same_zone(&self) -> bool {
        self.skew.abs() < 900
    }

    /// UTC seconds to server wall-clock seconds.
    fn wall(&self, t: i64) -> i64 {
        t + (self.local)(if self.same_zone() { t } else { self.now }) + self.skew
    }

    /// Server wall-clock seconds to UTC. A wall time skipped by the switch to summer time is the
    /// instant of the switch (02:30 that night → 03:00 summer time); one repeated by the switch
    /// back is its first occurrence (02:30 summer time).
    fn utc(&self, w: i64) -> i64 {
        if !self.same_zone() {
            return (w - (self.local)(self.now) - self.skew).max(0);
        }
        let x = w - self.skew; // local wall clock
        let (before, after) = ((self.local)(x - 86400), (self.local)(x + 86400));
        let fits = |o: i64| (self.local)(x - o) == o;
        let t = match (fits(before), fits(after)) {
            (true, true) => (x - before).min(x - after),
            (true, false) => x - before,
            (false, true) => x - after,
            (false, false) => {
                // In the gap: the switch lies between the two readings.
                let (mut lo, mut hi) = (x - before.max(after), x - before.min(after));
                let o = (self.local)(lo);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if (self.local)(mid) == o { lo = mid } else { hi = mid }
                }
                hi
            }
        };
        t.max(0)
    }
}

/// First console start after `checked` (UTC seconds), in UTC. A last check in the future (a
/// clock that was wrong when it was saved) counts as now.
fn due_after(sched: &crate::task::Schedule, checked: i64, clock: &Clock) -> Option<i64> {
    let checked = checked.min(clock.now);
    let mut w = clock.wall(checked);
    // A start that maps back to the last check or before it (a wall time skipped or repeated by a
    // daylight saving change) was already handled: the next one.
    for _ in 0..48 {
        let c = sched.next_after(w)?;
        let t = clock.utc(c);
        if t > checked {
            return Some(t);
        }
        w = c;
    }
    None
}

/// Next start from the console schedule (UTC seconds), counted from the last check. None until
/// the server's time zone is known.
pub fn console_due(task: &TaskInfo) -> Option<u64> {
    let skew = control::server_skew()?;
    let now = now() as i64;
    let checked = match load().checked {
        0 => now,
        c => c as i64,
    };
    due_after(&task.schedule, checked, &Clock { skew, local: crate::host::utc_offset, now }).map(|t| t as u64)
}

/// The server's offset (`get_timezone`), kept for the next runs; true if it answered. On failure
/// the last one known stays in use.
pub fn refresh_timezone(sess: &Session) -> bool {
    match sess.connect().and_then(|mut c| sess.server_offset(&mut c)) {
        Ok(off) => {
            let skew = skew_of(off, now() as i64, crate::host::utc_offset);
            control::set_server_skew(skew);
            let mut s = load();
            if s.skew != Some(skew) {
                s.skew = Some(skew);
                save(&s);
            }
            true
        }
        Err(e) => {
            control::net_error("get_timezone", &e);
            false
        }
    }
}

pub fn run(sess: Session, worker: Sender<LocalTrigger>) {
    let mut saved = load();
    let t = now();
    // Never checked, or checked "in the future" by a clock that was wrong (dead RTC battery).
    if saved.checked == 0 || saved.checked > t || saved.started == 0 {
        saved.checked = if saved.checked == 0 { t } else { saved.checked.min(t) };
        saved.started = if saved.started == 0 { t } else { saved.started };
        save(&saved);
    }
    let since = saved.started;
    let mut task: Option<TaskInfo> = None;
    let mut fetched: Option<Instant> = None;
    // The server's offset: again every hour by the wall clock (which counts a suspend, unlike
    // Instant), at once when the local offset changes, every 5 minutes while it cannot be had.
    let mut next_tz = 0;
    let mut tz_local = None;
    let mut waiting = false;
    loop {
        let t = now();
        let local = crate::host::utc_offset(t as i64);
        if t >= next_tz || t + 3600 < next_tz || tz_local != Some(local) {
            next_tz = t + if refresh_timezone(&sess) { 3600 } else { 300 };
            tz_local = Some(local);
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
            let local = local_due(&config::get().schedule, &LastRun::load(), since).filter(|d| d.0 <= t);
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
                save(&Saved { checked: t, ..load() });
                let _ = worker.send((TRIGGER_SCHEDULE, None));
            } else if let Some((_, trigger)) = local {
                eprintln!("scheduled backup ({})", if trigger.1.is_some() { "after boot" } else { "local schedule" });
                let _ = worker.send(trigger);
            }
        }
        std::thread::sleep(Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Schedule;

    /// Days from 1970-01-01 to a date of the proleptic Gregorian calendar.
    fn days(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
        era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
    }

    fn utc(y: i64, mo: i64, d: i64, h: i64, mi: i64) -> i64 {
        days(y, mo, d) * 86400 + h * 3600 + mi * 60
    }

    /// Europe/Rome whatever TZ the tests run under: CET, and CEST from the last Sunday of March
    /// 01:00 UTC to the last Sunday of October 01:00 UTC.
    fn rome(t: i64) -> i64 {
        let mut y = 1970 + t.div_euclid(31_556_952);
        while days(y, 1, 1) * 86400 > t {
            y -= 1;
        }
        while days(y + 1, 1, 1) * 86400 <= t {
            y += 1;
        }
        let last_sunday = |m| {
            let d = days(y, m, 31);
            d - (d + 4).rem_euclid(7)
        };
        let (start, end) = (last_sunday(3) * 86400 + 3600, last_sunday(10) * 86400 + 3600);
        if (start..end).contains(&t) { 7200 } else { 3600 }
    }

    fn daily(hour: u64, minute: u64) -> Schedule {
        Schedule { setting_type: 1, weekdays: (0..7).collect(), hour, minute, hourly: false, window: None }
    }

    fn clock(skew: i64, now: i64) -> Clock {
        Clock { skew, local: rome, now }
    }

    /// The service from `from` to `to`, a round every 30 s: when it starts a console backup.
    fn fires(sched: &Schedule, skew: i64, from: i64, to: i64) -> Vec<i64> {
        let mut checked = from;
        let mut out = Vec::new();
        for t in (from..to).step_by(30) {
            if due_after(sched, checked, &clock(skew, t)).is_some_and(|d| d <= t) {
                out.push(t);
                checked = t;
            }
        }
        out
    }

    #[test]
    fn rules_of_the_test_zone() {
        assert_eq!(rome(utc(2026, 3, 29, 0, 59)), 3600);
        assert_eq!(rome(utc(2026, 3, 29, 1, 0)), 7200);
        assert_eq!(rome(utc(2026, 10, 25, 0, 59)), 7200);
        assert_eq!(rome(utc(2026, 10, 25, 1, 0)), 3600);
        assert_eq!(rome(utc(2027, 1, 1, 0, 0)), 3600);
    }

    #[test]
    fn wall_times_around_the_switches() {
        let c = clock(0, 0);
        let wall = |y, mo, d, h, mi| utc(y, mo, d, h, mi); // the same arithmetic on wall clocks
        // Skipped (spring): the instant the clocks jump, 03:00 summer time.
        assert_eq!(c.utc(wall(2026, 3, 29, 2, 30)), utc(2026, 3, 29, 1, 0));
        assert_eq!(c.utc(wall(2026, 3, 29, 3, 30)), utc(2026, 3, 29, 1, 30));
        // Repeated (autumn): the first one, in summer time.
        assert_eq!(c.utc(wall(2026, 10, 25, 2, 30)), utc(2026, 10, 25, 0, 30));
        assert_eq!(c.utc(wall(2026, 10, 25, 3, 0)), utc(2026, 10, 25, 2, 0));
        assert_eq!(c.wall(utc(2026, 10, 25, 1, 30)), wall(2026, 10, 25, 2, 30));
    }

    #[test]
    fn normal_days() {
        let f = fires(&daily(3, 0), 0, utc(2026, 10, 19, 0, 0), utc(2026, 10, 21, 0, 0));
        assert_eq!(f, [utc(2026, 10, 19, 1, 0), utc(2026, 10, 20, 1, 0)]);
        // Winter: 03:00 is 02:00 UTC.
        let f = fires(&daily(3, 0), 0, utc(2026, 12, 1, 0, 0), utc(2026, 12, 2, 0, 0));
        assert_eq!(f, [utc(2026, 12, 1, 2, 0)]);
    }

    #[test]
    fn spring_gap_runs_once_when_the_clocks_jump() {
        // 02:30 does not exist on 2026-03-29: once, at 03:00 summer time (01:00 UTC).
        let f = fires(&daily(2, 30), 0, utc(2026, 3, 28, 0, 0), utc(2026, 3, 30, 6, 0));
        assert_eq!(f, [utc(2026, 3, 28, 1, 30), utc(2026, 3, 29, 1, 0), utc(2026, 3, 30, 0, 30)]);
        let f = fires(&daily(3, 0), 0, utc(2026, 3, 28, 0, 0), utc(2026, 3, 30, 6, 0));
        assert_eq!(f, [utc(2026, 3, 28, 2, 0), utc(2026, 3, 29, 1, 0), utc(2026, 3, 30, 1, 0)]);
    }

    #[test]
    fn fall_back_runs_once() {
        let f = fires(&daily(3, 0), 0, utc(2026, 10, 24, 0, 0), utc(2026, 10, 26, 6, 0));
        assert_eq!(f, [utc(2026, 10, 24, 1, 0), utc(2026, 10, 25, 2, 0), utc(2026, 10, 26, 2, 0)]);
        // 02:30 happens twice on 2026-10-25: the first one only.
        let f = fires(&daily(2, 30), 0, utc(2026, 10, 24, 0, 0), utc(2026, 10, 26, 6, 0));
        assert_eq!(f, [utc(2026, 10, 24, 0, 30), utc(2026, 10, 25, 0, 30), utc(2026, 10, 26, 1, 30)]);
    }

    #[test]
    fn offset_fetched_before_the_switch_still_holds_after_it() {
        // `get_timezone` answered +2 h on Saturday; Sunday the switch happens before the next
        // refresh. As an absolute offset it would now differ from the local one by an hour.
        let off = 7200;
        let skew = skew_of(off, utc(2026, 10, 24, 12, 0), rome);
        assert_eq!(skew, 0);
        assert_eq!(skew_of(3600, utc(2026, 10, 25, 12, 0), rome), skew);
        let f = fires(&daily(3, 0), skew, utc(2026, 10, 24, 12, 0), utc(2026, 10, 26, 6, 0));
        assert_eq!(f, [utc(2026, 10, 25, 2, 0), utc(2026, 10, 26, 2, 0)]);
    }

    #[test]
    fn server_in_another_time_zone() {
        // Server in New York (-4 h in October), agent in Rome (+2 h): 03:00 there is 07:00 UTC.
        let now = utc(2026, 10, 19, 0, 0);
        let skew = skew_of(-4 * 3600, now, rome);
        assert_eq!(due_after(&daily(3, 0), now, &clock(skew, now)), Some(utc(2026, 10, 19, 7, 0)));
    }

    #[test]
    fn last_check_in_the_future_counts_as_now() {
        let now = utc(2026, 10, 19, 0, 0);
        let future = utc(2036, 1, 1, 0, 0);
        assert_eq!(due_after(&daily(3, 0), future, &clock(0, now)), Some(utc(2026, 10, 19, 1, 0)));
    }

    #[test]
    fn interval_counts_from_the_last_attempt() {
        let sc = LocalSchedule { on_boot: false, interval_hours: 6, retry_minutes: 0, ..LocalSchedule::default() };
        // Failed (or rejected by the server: last_ok_start 0), no retries: still every 6 h.
        let failed = LastRun { start: 1000, status: "error".into(), ..LastRun::default() };
        assert_eq!(local_due(&sc, &failed, 500).map(|d| d.0), Some(1000 + 6 * 3600));
        let ok = LastRun { start: 1000, status: "complete".into(), last_ok_start: 1000, ..LastRun::default() };
        assert_eq!(local_due(&sc, &ok, 500).map(|d| d.0), Some(1000 + 6 * 3600));
        // Nothing yet, and no backup after boot: from the first start of the service.
        assert_eq!(local_due(&sc, &LastRun::default(), 500).map(|d| d.0), Some(500 + 6 * 3600));
        assert_eq!(local_due(&sc, &LastRun::default(), 0), None);
    }
}
