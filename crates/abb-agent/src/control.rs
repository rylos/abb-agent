//! State shared between the threads of the service and the running backup: cancellation,
//! bandwidth limit, backup window and the server clock.

use crate::task::{Schedule, ServerTime};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

static CANCEL: AtomicBool = AtomicBool::new(false);
/// SIGTERM, SIGINT or SIGHUP received: the running backup stops and cleans up, nothing new starts.
static STOPPING: AtomicBool = AtomicBool::new(false);
static BUSY: AtomicBool = AtomicBool::new(false);
static RUNNING_TASK: AtomicU64 = AtomicU64::new(0);
/// Bytes per second, 0 = unlimited.
static BANDWIDTH: AtomicU64 = AtomicU64::new(0);
/// Server UTC offset less the local one, plus the clock difference, in seconds (from
/// `get_timezone`); [`UNKNOWN`] until known.
static SERVER_SKEW: AtomicI64 = AtomicI64::new(UNKNOWN);
const UNKNOWN: i64 = i64::MIN;
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
    CANCEL.load(Ordering::SeqCst) || STOPPING.load(Ordering::SeqCst)
}

pub fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

extern "C" fn on_signal(sig: libc::c_int) {
    // Only an atomic store and async-signal-safe calls.
    STOPPING.store(true, Ordering::SeqCst);
    if sig == libc::SIGHUP {
        // The terminal is gone: writing to it would fail (and eprintln! panic) during the
        // cleanup. Its descriptors go to /dev/null; a pipe to the journal stays.
        unsafe {
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if null >= 0 {
                for fd in 0..=2 {
                    let mut st: libc::stat = std::mem::zeroed();
                    if libc::fstat(fd, &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFCHR {
                        libc::dup2(null, fd);
                    }
                }
                libc::close(null);
            }
        }
    }
}

/// SIGTERM (systemctl stop), SIGINT (Ctrl-C) and SIGHUP (the terminal closed) cancel the running
/// backup, which then removes its snapshots, thaws what it froze and tells the server, instead of
/// the process dying in the middle of it. System calls restart after the handler (SA_RESTART):
/// the backup notices the request at its next check, and waits poll it (a lock held by another
/// backup job, see `backup::flock`).
pub fn handle_stop_signals() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
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

pub fn set_server_skew(s: i64) {
    SERVER_SKEW.store(s, Ordering::SeqCst);
}

/// The server's UTC offset less the local one: as last fetched, or as the service saved it.
pub fn server_skew() -> Option<i64> {
    match SERVER_SKEW.load(Ordering::SeqCst) {
        UNKNOWN => {
            let s = crate::scheduler::saved_skew()?;
            let _ = SERVER_SKEW.compare_exchange(UNKNOWN, s, Ordering::SeqCst, Ordering::SeqCst);
            Some(s)
        }
        s => Some(s),
    }
}

/// The server's wall clock now (as the local one while its offset is unknown).
pub fn server_now() -> ServerTime {
    let t = crate::host::now() as i64;
    t + crate::host::utc_offset(t) + server_skew().unwrap_or(0)
}

static OFFLINE: AtomicBool = AtomicBool::new(false);

/// A request to the NAS failed. Network errors are logged once until the NAS answers again: a
/// laptop away from home would otherwise fill the journal.
pub fn net_error(what: &str, e: &abb_proto::transport::Error) {
    if matches!(e, abb_proto::transport::Error::Io(_)) {
        if !OFFLINE.swap(true, Ordering::SeqCst) {
            eprintln!("{what}: {e}: the NAS cannot be reached, retrying quietly");
        }
    } else {
        eprintln!("{what}: {e}");
    }
}

/// The NAS answered.
pub fn net_ok() {
    if OFFLINE.swap(false, Ordering::SeqCst) {
        eprintln!("the NAS can be reached again");
    }
}
