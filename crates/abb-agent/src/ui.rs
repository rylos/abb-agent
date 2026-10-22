//! Terminal output shared by `status` and `backup`: one header, one row layout, one set of marks
//! and the same progress line. On a terminal `abb-agent backup` shows its steps, a progress bar
//! and the outcome; the detailed log lines go to the journal (identifier `abb-agent`), as when the
//! service runs it, so `journalctl -t abb-agent` holds every backup.

use std::io::{BufRead, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// ANSI styles, only on a terminal and without `NO_COLOR`.
#[derive(Clone, Copy)]
pub struct Style(bool);

impl Style {
    pub fn for_fd(fd: RawFd) -> Style {
        Style(unsafe { libc::isatty(fd) } == 1 && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()))
    }
    fn paint(&self, code: &str, s: &str) -> String {
        if self.0 { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
}

/// The marks used by every command.
#[derive(Clone, Copy)]
pub enum Mark {
    /// ✓ done, complete.
    Ok,
    /// ! partial, cancelled, worth noticing.
    Warn,
    /// ✗ failed.
    Err,
    /// ▶ running.
    Run,
    /// ○ planned.
    Next,
    /// … under way.
    Busy,
}

impl Style {
    pub fn mark(&self, m: Mark) -> String {
        match m {
            Mark::Ok => self.green("✓"),
            Mark::Warn => self.yellow("!"),
            Mark::Err => self.red("✗"),
            Mark::Run => self.cyan("▶"),
            Mark::Next => self.dim("○"),
            Mark::Busy => self.dim("…"),
        }
    }

    /// Mark of a backup outcome (`complete`, `partial`, `cancel`, `error`).
    pub fn outcome(&self, status: &str) -> Mark {
        match status {
            "complete" => Mark::Ok,
            "partial" | "cancel" => Mark::Warn,
            _ => Mark::Err,
        }
    }

    pub fn outcome_color(&self, status: &str, s: &str) -> String {
        match self.outcome(status) {
            Mark::Ok => self.green(s),
            Mark::Warn => self.yellow(s),
            _ => self.red(s),
        }
    }

    /// `abb-agent 0.2.4  · Synology Active Backup for Business`
    pub fn header(&self) -> String {
        format!("{}  {}", self.bold(&format!("abb-agent {}", env!("CARGO_PKG_VERSION"))), self.dim("· Synology Active Backup for Business"))
    }

    /// `  Label        value`: a fact.
    pub fn row(&self, label: &str, value: &str) -> String {
        format!("  {}  {value}", self.dim(&format!("{label:<11}")))
    }

    /// `✓ Title  detail`: an event (a backup, its outcome).
    pub fn event(&self, m: Mark, title: &str, detail: &str) -> String {
        if detail.is_empty() { format!("{} {}", self.mark(m), self.bold(title)) } else { format!("{} {}  {detail}", self.mark(m), self.bold(title)) }
    }

    /// `  ████░░░░  42.1%   200.0 GiB of 475.0 GiB · 268.0 MiB/s · ~17 min left`
    pub fn progress(&self, done: u64, total: u64, rate: u64) -> String {
        let pct = if total == 0 { 100.0 } else { done.min(total) as f64 * 100.0 / total as f64 };
        let mut line = format!("  {} {:>5.1}%   {} of {}", self.cyan(&bar(pct, 30)), pct, size(done.min(total)), size(total));
        if rate > 0 {
            line += &format!(" {} {}/s", self.dim("·"), size(rate));
            let left = total.saturating_sub(done) / rate;
            if left > 0 {
                line += &format!(" {} ~{} left", self.dim("·"), fmt_duration(left));
            }
        }
        line
    }

    pub fn sep(&self) -> String {
        self.dim("·")
    }
}

pub fn bar(pct: f64, width: usize) -> String {
    let full = ((pct / 100.0 * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(full), "░".repeat(width - full))
}

pub fn size(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{b} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

pub fn fmt_duration(s: u64) -> String {
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        _ => format!("{} h {:02} min", s / 3600, s % 3600 / 60),
    }
}

/// The interactive view, while a backup runs on a terminal.
struct View {
    tty: std::fs::File,
    st: Style,
    /// A line being redrawn (step under way or progress bar).
    transient: bool,
    started: Instant,
    /// While sending: (done, total, bytes per second).
    progress: Option<(u64, u64, u64)>,
    forwarder: Option<std::thread::JoinHandle<()>>,
}

static VIEW: Mutex<Option<View>> = Mutex::new(None);

/// Starts the interactive view if stderr is a terminal and the journal is there: the terminal is
/// kept for the view, stderr (this process and the hooks it runs) goes to the journal.
pub fn start() -> bool {
    if unsafe { libc::isatty(2) } != 1 {
        return false;
    }
    let Ok(journal) = UnixDatagram::unbound() else { return false };
    if journal.connect("/run/systemd/journal/socket").is_err() {
        return false;
    }
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return false;
    }
    let tty_fd = unsafe { libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3) };
    if tty_fd < 0 || unsafe { libc::dup2(fds[1], 2) } < 0 {
        return false;
    }
    unsafe { libc::close(fds[1]) };
    let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let forwarder = std::thread::spawn(move || {
        for line in std::io::BufReader::new(reader).lines().map_while(Result::ok) {
            let msg = format!("SYSLOG_IDENTIFIER=abb-agent\nPRIORITY=6\nMESSAGE={line}\n");
            let _ = journal.send(msg.as_bytes());
        }
    });
    let st = Style::for_fd(tty_fd);
    let mut v = View {
        tty: unsafe { std::fs::File::from_raw_fd(tty_fd) },
        st,
        transient: false,
        started: Instant::now(),
        progress: None,
        forwarder: Some(forwarder),
    };
    v.line(&st.header());
    v.line("");
    *VIEW.lock().unwrap() = Some(v);
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let mut g = VIEW.lock().unwrap();
            let Some(v) = g.as_mut() else { return };
            if let Some((d, t, r)) = v.progress {
                let s = v.st.progress(d, t, r);
                v.redraw(&s);
            }
        }
    });
    true
}

impl View {
    fn line(&mut self, s: &str) {
        if std::mem::take(&mut self.transient) {
            let _ = write!(self.tty, "\r\x1b[2K");
        }
        let _ = writeln!(self.tty, "{s}");
    }

    fn redraw(&mut self, s: &str) {
        let _ = write!(self.tty, "\r\x1b[2K{s}");
        let _ = self.tty.flush();
        self.transient = true;
    }
}

fn with(f: impl FnOnce(&mut View)) {
    if let Some(v) = VIEW.lock().unwrap().as_mut() {
        f(v);
    }
}

/// A fact about the backup (`Server`, `Task`, `Disks`…), in the layout of `status`.
pub fn row(label: &str, value: &str) {
    with(|v| {
        let s = v.st.row(label, value);
        v.line(&s);
    });
}

/// A blank line between groups.
pub fn gap() {
    with(|v| v.line(""));
}

/// The backup started: `▶ Backing up  incremental, version 6`.
pub fn running(detail: &str) {
    with(|v| {
        let s = v.st.event(Mark::Run, "Backing up", detail);
        v.line(&s);
    });
}

/// A phase under way, redrawn in place until the next line.
pub fn busy(text: &str) {
    with(|v| {
        let s = format!("  {} {}", v.st.mark(Mark::Busy), v.st.dim(text));
        v.redraw(&s);
    });
}

/// A phase done, or something worth noticing.
pub fn step(m: Mark, text: &str) {
    with(|v| {
        let s = format!("  {} {text}", v.st.mark(m));
        v.line(&s);
    });
}

/// Progress of the upload: bytes processed, total, bytes per second.
pub fn progress(done: u64, total: u64, rate: u64) {
    with(|v| {
        let rate = if rate == 0 { v.progress.map_or(0, |p| p.2) } else { rate };
        v.progress = Some((done, total, rate));
    });
}

/// The upload ended: the bar gives way to a step line.
pub fn sent(text: &str) {
    with(|v| {
        v.progress = None;
        let s = format!("  {} {text}", v.st.mark(Mark::Ok));
        v.line(&s);
    });
}

/// Ends the view with the outcome, in the layout of `status` ("Last backup"), and gives stderr
/// back to the terminal once the log lines still in the pipe have reached the journal.
pub fn finish(status: &str, detail: &str) {
    let Some(mut v) = VIEW.lock().unwrap().take() else { return };
    v.progress = None;
    let st = v.st;
    let elapsed = fmt_duration(v.started.elapsed().as_secs());
    let mut d = format!("{} {} {elapsed}", st.outcome_color(status, status), st.sep());
    if !detail.is_empty() {
        d = format!("{detail} {} {d}", st.sep());
    }
    v.line("");
    v.line(&st.event(st.outcome(status), "Backup", &d));
    if status != "complete" {
        v.line(&st.row("Details", "journalctl -t abb-agent -e"));
    }
    // Back to the terminal: this closes the pipe, so the forwarder drains it and stops.
    unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&v.tty), 2) };
    if let Some(f) = v.forwarder.take() {
        let _ = f.join();
    }
}
