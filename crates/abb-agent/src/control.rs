//! State shared between the threads of the service and the running backup: cancellation,
//! bandwidth limit, backup window and the server clock.

use crate::task::{Schedule, ServerTime};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

static CANCEL: AtomicBool = AtomicBool::new(false);
static BUSY: AtomicBool = AtomicBool::new(false);
static RUNNING_TASK: AtomicU64 = AtomicU64::new(0);
/// Bytes per second, 0 = unlimited.
static BANDWIDTH: AtomicU64 = AtomicU64::new(0);
/// Server UTC offset plus clock difference, in seconds (from `get_timezone`).
static SERVER_OFFSET: AtomicI64 = AtomicI64::new(0);
static WINDOW: Mutex<Option<Schedule>> = Mutex::new(None);
static TASKS_CHANGED: AtomicBool = AtomicBool::new(false);

/// The task settings changed on the server (event `change_task`/`update_task`).
pub fn tasks_changed() {
    TASKS_CHANGED.store(true, Ordering::SeqCst);
}

pub fn take_tasks_changed() -> bool {
    TASKS_CHANGED.swap(false, Ordering::SeqCst)
}

/// A backup starts: `running_task` on the server, and the task schedule whose backup window
/// must be respected while it runs.
pub fn begin(running_task: u64, schedule: Option<Schedule>) {
    CANCEL.store(false, Ordering::SeqCst);
    RUNNING_TASK.store(running_task, Ordering::SeqCst);
    *WINDOW.lock().unwrap() = schedule.filter(|s| s.window.is_some());
}

pub fn end() {
    RUNNING_TASK.store(0, Ordering::SeqCst);
    *WINDOW.lock().unwrap() = None;
}

pub fn running_task() -> u64 {
    RUNNING_TASK.load(Ordering::SeqCst)
}

pub fn set_busy(b: bool) {
    BUSY.store(b, Ordering::SeqCst);
}

pub fn busy() -> bool {
    BUSY.load(Ordering::SeqCst)
}

pub fn cancel(reason: &str) {
    if !CANCEL.swap(true, Ordering::SeqCst) {
        eprintln!("cancelling the backup: {reason}");
    }
}

pub fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

/// The running backup must stop: cancelled, or outside the backup window.
pub fn should_stop() -> bool {
    if cancelled() {
        return true;
    }
    let outside = WINDOW.lock().unwrap().as_ref().is_some_and(|s| !s.allows(server_now()));
    if outside {
        cancel("outside the backup window");
    }
    outside
}

pub fn set_bandwidth(bytes_per_s: u64) {
    if BANDWIDTH.swap(bytes_per_s, Ordering::SeqCst) != bytes_per_s {
        eprintln!(
            "bandwidth limit: {}",
            if bytes_per_s == 0 { "none".into() } else { format!("{} KiB/s", bytes_per_s / 1024) }
        );
    }
}

pub fn bandwidth() -> u64 {
    BANDWIDTH.load(Ordering::SeqCst)
}

pub fn set_server_offset(s: i64) {
    SERVER_OFFSET.store(s, Ordering::SeqCst);
}

pub fn server_offset() -> i64 {
    SERVER_OFFSET.load(Ordering::SeqCst)
}

pub fn server_now() -> ServerTime {
    crate::host::now() as i64 + server_offset()
}
