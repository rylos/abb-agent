//! abb-agent: Linux client for Synology Active Backup for Business, with image backup
//! of btrfs volumes.

mod backup;
mod config;
mod control;
mod disk;
mod filerestore;
mod fixup;
mod hooks;
mod host;
mod nodes;
mod scheduler;
mod scripts;
mod session;
mod stream;
mod task;
mod volume;
mod state;
mod ui;
mod version;

use abb_proto::transport::{DEFAULT_PORT, fingerprint_hex};
use abb_proto::{Connection, Value, request};
use session::Session;
use state::State;
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  abb-agent connect -a <server> -u <user> [-p <port>] [--otp <code>]
                                                         pair this computer with the ABB server
                                                         (password from ABB_PASSWORD or prompted,
                                                         two-factor code prompted when needed)
  abb-agent status [-w]                                  connection and task status (-w: follow a
                                                         running backup until it ends)
  abb-agent daemon                                       service: heartbeat, events, scheduled backups
  abb-agent backup [-v]                                  back up now, in the foreground (-v: the log here,
                                                         not the journal)
  abb-agent cleanup                                      delete snapshots left by an interrupted backup
  abb-agent config                                       configuration in effect (/etc/abb-agent.toml)
  abb-agent disks [disk]                                 disks, partitions and filesystems
  abb-agent bench [disk…] [--limit GiB] [--image PREFIX]
                                                         offline test: snapshot, read,
                                                         verify and compress the whole disk
  abb-agent probe <server>                               server version, without pairing
  abb-agent version                                      agent version";

fn main() {
    let all: Vec<String> = std::env::args().collect();
    let cmd = all.get(1).map_or("", String::as_str);
    // What each command accepts: flags, options with a value, at most so many other words.
    let (flags, opts, words): (&[&str], &[&str], usize) = match cmd {
        "connect" => (&[], &["-a", "-u", "-p", "--otp"], 0),
        "status" => (&["-w", "--watch"], &[], 0),
        "backup" => (&["-v", "--verbose"], &[], 0),
        "disks" | "probe" => (&[], &[], 1),
        "bench" => (&[], &["--limit", "--image"], usize::MAX),
        "config" | "restore-fixup" | "version" | "--version" | "-V" | "daemon" | "cleanup" => (&[], &[], 0),
        _ => usage(),
    };
    let args = Args::parse(&all[2..], flags, opts, words).unwrap_or_else(|e| {
        eprintln!("abb-agent {cmd}: {e}");
        usage()
    });
    match cmd {
        // Neither depends on the configuration: early at boot (abb-agent-fixup.service) the
        // fixup must run even if /etc/abb-agent.toml has an error.
        "restore-fixup" => return fixup::run(),
        "version" | "--version" | "-V" => return println!("abb-agent {}", env!("CARGO_PKG_VERSION")),
        _ => {}
    }
    let cfg = config::load().unwrap_or_else(|e| die(format!("configuration: {e}")));
    version::apply_config().unwrap_or_else(|e| die(format!("configuration: {e}")));
    if matches!(cmd, "config" | "status" | "disks") {
        // Output piped into `head` and the like: end quietly when the reader goes away.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match cmd {
        "config" => {
            println!("# {}", config::path());
            print!("{}", toml::to_string_pretty(cfg).unwrap_or_else(|e| die(e)));
        }
        "connect" => {
            let server = args.opt("-a").unwrap_or_else(|| usage()).to_string();
            let user = args.opt("-u").unwrap_or_else(|| usage()).to_string();
            let port = args.opt("-p").map_or(DEFAULT_PORT, |p| p.parse().unwrap_or_else(|_| usage()));
            let _instance =
                instance_lock("the abb-agent service is running: stop it first (sudo systemctl stop abb-agent), pair, then start it again");
            let password = std::env::var("ABB_PASSWORD").unwrap_or_else(|_| prompt_password(&user));
            let otp = args.opt("--otp");
            let state = match session::pair(&server, port, &user, &password, otp) {
                // 0x1005: the account has two-factor authentication and no code was given.
                Err(abb_proto::Error::Server { code: 0x1005, .. }) if otp.is_none() => {
                    eprint!("two-factor code for {user}: ");
                    let _ = std::io::stderr().flush();
                    let mut code = String::new();
                    let _ = std::io::stdin().lock().read_line(&mut code);
                    session::pair(&server, port, &user, &password, Some(code.trim()))
                }
                r => r,
            }
            .unwrap_or_else(|e| die(e));
            state.save().unwrap_or_else(|e| die(e));
            println!("paired with {server} as {user}: the device and its task are in the ABB console");
        }
        "status" => status(args.flag("-w") || args.flag("--watch")),
        "cleanup" => backup::startup_cleanup(),
        "backup" => {
            control::handle_stop_signals();
            let sess = load();
            // On a terminal: steps and progress bar, the log to the journal; -v keeps the log here.
            let view = !(args.flag("-v") || args.flag("--verbose")) && ui::start();
            let res = sess.auth_by_token(false).and_then(|()| {
                let trigger = backup::Trigger::Local { trigger_type: backup::TRIGGER_MANUAL, trigger_event: None };
                backup::run(&sess, trigger)
            });
            if let Err(e) = res {
                if view {
                    ui::finish("error", &e.to_string());
                    std::process::exit(1);
                }
                die(e);
            }
        }
        "daemon" => daemon(),
        "disks" => {
            let name = args.words.first().cloned().unwrap_or_else(|| disk::system_disk().unwrap_or_else(|e| die(e)));
            let d = disk::scan(&name).unwrap_or_else(|e| die(e));
            println!("{} {} bytes, sector {}, {}", d.name, d.size, d.sector_size, d.layout);
            for p in &d.partitions {
                println!(
                    "  p{:<2} {:>14} +{:>14} {:<6} {:<38} attr {:>2} {:?}",
                    p.number,
                    p.offset,
                    p.length,
                    p.fs.name(),
                    p.fs_uuid,
                    p.attribute(),
                    p.mount_path
                );
            }
        }
        "bench" => {
            let limit = args.opt("--limit").map(|g| g.parse::<u64>().unwrap_or_else(|_| usage()) << 30);
            let names = if args.words.is_empty() { vec![disk::system_disk().unwrap_or_else(|e| die(e))] } else { args.words.clone() };
            backup::bench(names, limit, args.opt("--image")).unwrap_or_else(|e| die(e));
        }
        "probe" => {
            let host = args.words.first().cloned().unwrap_or_else(|| usage());
            let mut c = Connection::open(&host, DEFAULT_PORT, None).unwrap_or_else(|e| die(e));
            println!("certificate: {}", fingerprint_hex(&c.fingerprint));
            let r = c.call(&request("query_server_info", vec![])).unwrap_or_else(|e| die(e));
            println!("{r:#?}");
        }
        _ => usage(),
    }
}

/// The arguments after the command.
#[derive(Debug, Default)]
struct Args {
    flags: Vec<String>,
    opts: Vec<(String, String)>,
    words: Vec<String>,
}

impl Args {
    /// Anything the command does not accept is an error: `backup --dry-run` must not back up.
    fn parse(args: &[String], flags: &[&str], opts: &[&str], max_words: usize) -> Result<Args, String> {
        let mut out = Args::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if flags.contains(&a.as_str()) {
                out.flags.push(a.clone());
            } else if opts.contains(&a.as_str()) {
                let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
                out.opts.push((a.clone(), v.clone()));
            } else if a.starts_with('-') && a.len() > 1 {
                return Err(format!("unknown option {a}"));
            } else if out.words.len() < max_words {
                out.words.push(a.clone());
            } else {
                return Err(format!("unexpected argument {a}"));
            }
        }
        Ok(out)
    }

    fn flag(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }

    fn opt(&self, o: &str) -> Option<&str> {
        self.opts.iter().rev().find(|(k, _)| k == o).map(|(_, v)| v.as_str())
    }
}

/// One `daemon` or `connect` at a time: both write the agent state, and a pairing made while
/// the service runs would be overwritten by it. The lock is a root-only file in the state
/// directory, held until the process ends; `busy` is the error when another process holds it.
fn instance_lock(busy: &str) -> std::fs::File {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = std::path::Path::new(state::STATE_DIR);
    let path = dir.join("instance.lock");
    let fail = |e: std::io::Error| -> ! {
        match e.kind() {
            std::io::ErrorKind::PermissionDenied => die("permission denied: run it with sudo"),
            std::io::ErrorKind::WouldBlock => die(busy),
            _ => die(format!("{}: {e}", path.display())),
        }
    };
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).unwrap_or_else(|e| fail(e));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .unwrap_or_else(|e| fail(e));
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        fail(std::io::Error::last_os_error());
    }
    f
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

fn die(e: impl std::fmt::Display) -> ! {
    eprintln!("error: {e}");
    std::process::exit(1);
}

/// Terminal settings to put back if a signal ends the process at the password prompt.
static SAVED_TERM: std::sync::atomic::AtomicPtr<libc::termios> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

extern "C" fn restore_term(sig: libc::c_int) {
    // Async-signal-safe calls only; then the signal's default action (the process ends).
    let t = SAVED_TERM.load(std::sync::atomic::Ordering::SeqCst);
    unsafe {
        if !t.is_null() {
            libc::tcsetattr(0, libc::TCSANOW, t);
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

const PROMPT_SIGNALS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// Echo off on the terminal while it lives; back on when dropped or when Ctrl-C ends the process.
struct NoEcho(Box<libc::termios>);

impl NoEcho {
    fn new(fd: libc::c_int) -> Option<NoEcho> {
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
            return None;
        }
        let mut saved = NoEcho(Box::new(term));
        SAVED_TERM.store(&mut *saved.0, std::sync::atomic::Ordering::SeqCst);
        let handler = restore_term as extern "C" fn(libc::c_int) as libc::sighandler_t;
        for sig in PROMPT_SIGNALS {
            unsafe { libc::signal(sig, handler) };
        }
        let mut quiet = term;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
        Some(saved)
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &*self.0) };
        SAVED_TERM.store(std::ptr::null_mut(), std::sync::atomic::Ordering::SeqCst);
        for sig in PROMPT_SIGNALS {
            unsafe { libc::signal(sig, libc::SIG_DFL) };
        }
    }
}

fn prompt_password(user: &str) -> String {
    eprint!("password for {user}: ");
    let _ = std::io::stderr().flush();
    let quiet = NoEcho::new(0);
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if quiet.is_some() {
        drop(quiet);
        eprintln!();
    }
    line.trim_end_matches(['\n', '\r']).to_string()
}

fn load() -> Session {
    let state = State::load().unwrap_or_else(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => die("permission denied: run it with sudo"),
        _ => die(format!("cannot read state ({e}): run `abb-agent connect` first")),
    });
    Session::new(state)
}

/// The live line of a running backup: progress bar while sending, otherwise the phase.
fn progress_line(st: &ui::Style) -> Option<String> {
    let p = state::Progress::load()?;
    Some(match p.phase.as_str() {
        "sending" if p.total > 0 => st.progress(p.done, p.total, p.rate),
        "finishing" => format!("  {} {}", st.mark(ui::Mark::Busy), st.dim("all sent, the server is checking the version")),
        _ => format!("  {} {}", st.mark(ui::Mark::Busy), st.dim("preparing: snapshots and plan of what to send")),
    })
}

fn last_backup_line(st: &ui::Style, last: &state::LastRun) -> String {
    let sep = format!(" {} ", st.sep());
    let server = if last.server_status.is_empty() || last.server_status == last.status {
        String::new()
    } else {
        format!("{sep}on the server: {}", last.server_status)
    };
    let detail = format!(
        "{} → {} ({}){sep}{}{server}",
        fmt_short(last.start),
        fmt_short(last.end),
        ui::fmt_duration(last.end.saturating_sub(last.start)),
        st.outcome_color(&last.status, &last.status)
    );
    st.event(st.outcome(&last.status), "Last backup", &detail)
}

/// `status -w`: redraws the progress line in place until the backup ends, then shows how.
fn follow(st: &ui::Style) {
    println!();
    let mut out = std::io::stdout();
    while let Some(line) = progress_line(st) {
        let _ = write!(out, "\r\x1b[2K{line}");
        let _ = out.flush();
        std::thread::sleep(Duration::from_secs(1));
    }
    let _ = write!(out, "\r\x1b[2K");
    // The backup process records its outcome right after clearing the progress.
    let mut last = state::LastRun::load();
    for _ in 0..20 {
        if last.status != "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
        last = state::LastRun::load();
    }
    if last.status == "running" {
        println!("{}", st.event(ui::Mark::Warn, "Backup", "the backup process ended without recording an outcome"));
    } else {
        println!("{}", last_backup_line(st, &last));
    }
}

fn status(watch: bool) {
    let st = ui::Style::for_fd(1);
    let sess = load();
    let sep = format!(" {} ", st.sep());
    println!("{}", st.header());
    println!();
    let package = version::query(&sess);
    let nas = match &package {
        Ok(p) => format!("{sep}Active Backup {p}"),
        Err(_) => String::new(),
    };
    println!("{}", st.row("Server", &format!("{}:{}{sep}user {}{nas}", sess.state.server, sess.state.port, sess.state.username)));
    println!("{}", st.row("Device", &sess.state.device_uuid));
    if let Err(e) = sess.auth_by_token(false) {
        match version::refusal(&e) {
            Some(why) => {
                println!();
                println!("{}", st.event(ui::Mark::Err, "Refused", &why));
                std::process::exit(1);
            }
            None => die(e),
        }
    }
    let mut c = sess.connect().unwrap_or_else(|e| die(e));
    let tasks = sess.tasks(&mut c).unwrap_or_else(|e| die(e));
    let sc = &config::get().schedule;
    for t in &tasks {
        println!();
        let id = t.get("id").and_then(|v| v.as_int()).unwrap_or(0);
        let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
        println!("{}", st.row("Task", &format!("#{id} {}", st.bold(name))));
        if let Some(info) = task::TaskInfo::parse(t) {
            let sch = &info.schedule;
            let console = if sch.is_scheduled() {
                format!(
                    "{} at {:02}:{:02} (server time){}{}",
                    weekdays(&sch.weekdays),
                    sch.hour,
                    sch.minute,
                    if sch.hourly { ", then every hour" } else { "" },
                    if sch.window.is_some() { ", within the backup window" } else { "" }
                )
            } else {
                "manual only".into()
            };
            println!("{}", st.row("Console", &console));
        }
    }
    let mut local = vec![format!("after boot: {}", if sc.on_boot { "yes" } else { "no" })];
    if sc.interval_hours > 0 {
        local.push(format!("every {} h", sc.interval_hours));
    }
    local.push(if sc.retry_minutes > 0 { format!("retry after {} min", sc.retry_minutes) } else { "no retry".into() });
    if !sc.only_if.trim().is_empty() {
        local.push(if hooks::allowed() { "only_if holds now".into() } else { "only_if does not hold now".into() });
    }
    println!("{}", st.row("Local", &local.join(&sep)));
    // The disks a backup would take, as the backup shows them.
    let cfg_disks = &config::get().disks;
    let names: Vec<String> = if cfg_disks.is_empty() {
        disk::internal_disks().unwrap_or_default()
    } else {
        cfg_disks.iter().map(|e| disk::resolve(e).unwrap_or_else(|_| e.clone())).collect()
    };
    let disks: Vec<String> = names
        .iter()
        .map(|n| match disk::scan(n) {
            Ok(d) => disk::label(&d),
            Err(_) if !disk::has_partitions(n) => format!("{n} (not included: no partitions)"),
            Err(e) => format!("{n} ({e})"),
        })
        .collect();
    println!("{}", st.row("Disks", &disks.join(", ")));
    if let Some(info) = tasks.first().and_then(task::TaskInfo::parse) {
        println!("{}", st.row("Volumes", &info.selection.volumes()));
    }

    let running = sess.running_tasks(&mut c).unwrap_or_else(|e| die(e));
    let last = state::LastRun::load();
    println!();
    if last.status == "running" {
        let ago = host::now().saturating_sub(last.start);
        println!("{}", st.event(ui::Mark::Run, "Backing up", &format!("started {} ({} ago)", fmt_short(last.start), ui::fmt_duration(ago))));
        if !watch && let Some(line) = progress_line(&st) {
            println!("{line}");
        }
    } else if !running.is_empty() {
        let what = if running.iter().any(|r| r.status == 1) { "running" } else { "queued" };
        println!("{}", st.event(ui::Mark::Run, &format!("Backup {what} on the server"), ""));
    }
    if last.status != "running" && last.start != 0 {
        println!("{}", last_backup_line(&st, &last));
    }
    if last.last_ok_start != 0 && last.status != "complete" {
        println!("{}", st.row("Last success", &format!("started {}", fmt_short(last.last_ok_start))));
    }
    if let Ok(off) = sess.server_offset(&mut c) {
        control::set_server_skew(scheduler::skew_of(off, host::now() as i64, host::utc_offset));
    }
    let console = tasks.first().and_then(task::TaskInfo::parse).and_then(|t| scheduler::console_due(&t));
    let local = scheduler::local_due(sc, &last, scheduler::started()).map(|d| d.0.max(host::now()));
    match [console, local].into_iter().flatten().min() {
        _ if last.status == "running" || !running.is_empty() => {}
        Some(t) => println!("{}", st.event(ui::Mark::Next, "Next backup", &fmt_short(t))),
        None => println!("{}", st.event(ui::Mark::Next, "Next backup", "when started from the console or with `abb-agent backup`")),
    }
    let warnings = package.as_ref().map(version::warnings).unwrap_or_default();
    if !warnings.is_empty() {
        println!();
    }
    for w in warnings {
        println!("{}", st.event(ui::Mark::Warn, "Version", &w));
    }
    if watch && last.status == "running" {
        follow(&st);
    }
}

/// Days of the week (0 = Sunday) as "Mon–Fri", "every day", "Mon, Wed, Fri".
fn weekdays(days: &[u64]) -> String {
    const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    // Monday first.
    let mut d: Vec<u64> = days.iter().map(|&x| (x + 6) % 7).collect();
    d.sort_unstable();
    d.dedup();
    if d.len() == 7 {
        return "every day".into();
    }
    let name = |x: u64| NAMES[((x + 1) % 7) as usize];
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < d.len() {
        let mut j = i;
        while j + 1 < d.len() && d[j + 1] == d[j] + 1 {
            j += 1;
        }
        out.push(if j - i >= 2 { format!("{}–{}", name(d[i]), name(d[j])) } else { (i..=j).map(|k| name(d[k])).collect::<Vec<_>>().join(", ") });
        i = j + 1;
    }
    out.join(", ")
}

/// "today 13:50", "yesterday 11:07", "Thu 23/10 03:00".
fn fmt_short(t: u64) -> String {
    if t == 0 {
        return "-".into();
    }
    let tm = |t: u64| {
        let tt = t as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&tt, &mut tm) };
        tm
    };
    let (a, now) = (tm(t), tm(host::now()));
    let day = (a.tm_year, a.tm_yday);
    let hm = format!("{:02}:{:02}", a.tm_hour, a.tm_min);
    if day == (now.tm_year, now.tm_yday) {
        format!("today {hm}")
    } else if day == (now.tm_year, now.tm_yday - 1) {
        format!("yesterday {hm}")
    } else if day == (now.tm_year, now.tm_yday + 1) {
        format!("tomorrow {hm}")
    } else {
        const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        format!("{} {:04}-{:02}-{:02} {hm}", NAMES[a.tm_wday as usize], a.tm_year + 1900, a.tm_mon + 1, a.tm_mday)
    }
}

/// Service: heartbeat, schedule and backups each in a thread of their own; the main thread
/// handles the server's events, also while a backup runs (cancel, bandwidth, console requests).
fn daemon() {
    let _instance = instance_lock("another abb-agent daemon (or `abb-agent connect`) is running");
    let sess = load();
    control::handle_stop_signals();
    backup::startup_cleanup();
    // On SIGTERM: wait for the running backup to cancel and clean up, then exit.
    spawn_vital("stop", || {
        loop {
            std::thread::sleep(Duration::from_millis(200));
            if control::stopping() && !control::busy() {
                eprintln!("stopped");
                std::process::exit(0);
            }
        }
    });
    version::check_server(&sess);
    loop {
        match sess.auth_by_token(true) {
            Ok(()) => break,
            Err(e) => match version::refusal(&e) {
                // Retrying soon does not help: once an hour, and the reason in `status`.
                Some(why) => {
                    if version::refused(&why) {
                        eprintln!("auth: {why}; retrying every hour");
                    }
                    std::thread::sleep(Duration::from_secs(3600));
                    version::check_server(&sess);
                }
                None => {
                    control::net_error("auth", &e);
                    std::thread::sleep(Duration::from_secs(30));
                }
            },
        }
    }
    version::accepted();
    control::net_ok();
    eprintln!("connected to {} as {}", sess.state.server, sess.host.host_name);
    // A backup interrupted by a crash or power loss is still "running" here: take its outcome
    // from the server before scheduling anything.
    fetch_result(&sess);

    let hb_state = sess.state.clone();
    spawn_vital("heartbeat", move || {
        let s = Session::new(hb_state);
        let mut conn: Option<Connection> = None;
        let mut last_auth = Instant::now();
        let mut last_check = Instant::now();
        let mut auth_failed = false;
        loop {
            std::thread::sleep(Duration::from_secs(6));
            if conn.is_none() {
                conn = s.connect().map_err(|e| control::net_error("heartbeat", &e)).ok();
            }
            if let Some(c) = conn.as_mut()
                && let Err(e) = s.heart_beat(c)
            {
                if version::refusal(&e).is_none() {
                    control::net_error("heartbeat", &e);
                }
                conn = None;
            }
            if last_auth.elapsed() > Duration::from_secs(60) {
                match s.auth_by_token(false) {
                    Ok(()) => {
                        version::accepted();
                        control::net_ok();
                        // Back after an outage: the NAS may have updated Active Backup.
                        if auth_failed || last_check.elapsed() > Duration::from_secs(6 * 3600) {
                            version::check_server(&s);
                            last_check = Instant::now();
                        }
                        auth_failed = false;
                    }
                    Err(e) => {
                        match version::refusal(&e) {
                            Some(why) if version::refused(&why) => eprintln!("auth renewal: {why}"),
                            Some(_) => {}
                            None => control::net_error("auth renewal", &e),
                        }
                        auth_failed = true;
                    }
                }
                last_auth = Instant::now();
            }
        }
    });

    // Backups run one at a time in the worker: local triggers come from the scheduler, `None`
    // means "run what the server has queued".
    let (tx, rx) = std::sync::mpsc::channel::<Option<scheduler::LocalTrigger>>();
    let worker_state = sess.state.clone();
    spawn_vital("worker", move || {
        let s = Session::new(worker_state);
        for req in rx {
            if control::stopping() {
                continue;
            }
            control::set_busy(true);
            if let Some((trigger_type, trigger_event)) = req {
                let last = state::LastRun::load();
                if let Err(e) = backup::run(&s, backup::Trigger::Local { trigger_type, trigger_event }) {
                    eprintln!("backup: {e}");
                    // Failed before starting (e.g. server unreachable): retry later. Not when
                    // a backup started by hand holds the lock: that one is running.
                    let mut l = state::LastRun::load();
                    if l.start == last.start && !backup::in_progress() {
                        l.start = host::now();
                        l.status = "error".into();
                        let _ = l.save();
                    }
                }
            }
            run_pending(&s);
            control::set_busy(false);
        }
    });

    let sched_state = sess.state.clone();
    let sched_tx = tx.clone();
    spawn_vital("scheduler", move || {
        let (stx, srx) = std::sync::mpsc::channel();
        spawn_vital("scheduler queue", move || {
            for t in srx {
                let _ = sched_tx.send(Some(t));
            }
        });
        scheduler::run(Session::new(sched_state), stx)
    });

    let mut sess = sess;
    let mut poll: Option<Connection> = None;
    // A backup or a file restore may already be queued at startup.
    let _ = tx.send(None);
    filerestore::check(&sess);
    loop {
        if poll.is_none() {
            match sess.connect() {
                Ok(c) => {
                    let _ = c.set_read_timeout(Some(Duration::from_secs(450)));
                    poll = Some(c);
                }
                Err(e) => {
                    control::net_error("long_poll", &e);
                    std::thread::sleep(Duration::from_secs(15));
                    continue;
                }
            }
        }
        let c = poll.as_mut().unwrap();
        let has = match sess.long_poll(c, 400) {
            Ok(h) => h,
            Err(e) => {
                // The server sometimes lets the wait run past our read timeout: just reconnect.
                let timeout = matches!(&e, abb_proto::transport::Error::Io(io)
                    if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));
                if version::refusal(&e).is_some() {
                    // Logged by the auth renewal; wait instead of asking again at once.
                    std::thread::sleep(Duration::from_secs(60));
                } else if !timeout {
                    control::net_error("long_poll", &e);
                }
                poll = None;
                continue;
            }
        };
        if !has {
            continue;
        }
        let events = match sess.pull_events(c) {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!("pull_event: {e}");
                poll = None;
                continue;
            }
        };
        let mut check_tasks = false;
        for ev in &events {
            eprintln!("event {}: {} (task {})", ev.id, ev.action, ev.task_id);
            match ev.action.as_str() {
                "change_task" | "update_task" => {
                    control::tasks_changed();
                    check_tasks = true;
                }
                "update_running_task" => {
                    check_tasks = true;
                    filerestore::check(&sess);
                }
                "update_backup_bandwidth" => {
                    match sess.connect().and_then(|mut c| sess.call(&mut c, "get_backup_bandwidth", vec![])) {
                        Ok(r) => control::set_bandwidth(r.get("backup_bandwidth").and_then(Value::as_int).unwrap_or(0)),
                        Err(e) => eprintln!("get_backup_bandwidth: {e}"),
                    }
                }
                // Requests from the ABB console that need an answer (e.g. the volume list).
                "act_client" => nodes::answer(&sess, ev),
                // The server finished processing a version (verification included).
                "get_running_task_result" => fetch_result(&sess),
                "update_agent" => version::update_requested(&sess),
                _ => {}
            }
        }
        for ev in &events {
            if let Err(e) = sess.done_event(c, ev.id) {
                eprintln!("done_event {}: {e}", ev.id);
            }
            sess.state.last_event_id = sess.state.last_event_id.max(ev.id);
        }
        sess.state.token = session::token(&sess.state);
        let _ = sess.state.save();
        if check_tasks {
            if control::busy() {
                check_cancel(&sess);
            } else {
                let _ = tx.send(None);
            }
        }
    }
}

/// Starts a thread the service cannot do without. If it panics the process ends with a failure
/// (and systemd starts it again): a service whose worker or scheduler died would stay up doing
/// nothing, marked busy, and hang on stop. Panics caught inside it (the read pipeline) are not
/// concerned.
fn spawn_vital(name: &'static str, f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
            // Not eprintln!: if writing to stderr is what panicked, it must not stop the exit.
            let _ = writeln!(std::io::stderr(), "{name} thread panicked: exiting so that the service starts again");
            std::process::exit(1);
        }
    });
}

/// Final status of our last backup on the server. A version the server rejects after the
/// upload (e.g. its verification failed) counts as a failed backup, so it is retried.
fn fetch_result(sess: &Session) {
    let mut last = state::LastRun::load();
    if last.result_id == 0 {
        return;
    }
    let r = sess.connect().and_then(|mut c| {
        sess.call(&mut c, "get_running_task_result", vec![("result_id", Value::Int(last.result_id))])
    });
    let status = match &r {
        Ok(r) => r
            .get("running_task_result")
            .and_then(|t| t.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Err(e) => {
            eprintln!("get_running_task_result: {e}");
            return;
        }
    };
    let stale = last.status == "running" && !control::busy() && !backup::in_progress();
    if status.is_empty() || (status == last.server_status && !stale) {
        return;
    }
    if status != last.server_status {
        eprintln!("result {} on the server: {status}", last.result_id);
    }
    last.server_status = status.clone();
    if stale {
        // The process that ran this backup ended without recording how (killed, power loss):
        // the server's outcome is the one that counts.
        let local = match status.as_str() {
            "complete" | "success" => Some("complete"),
            "partial" => Some("partial"),
            "cancel" => Some("cancel"),
            "failure" | "error" => Some("error"),
            _ => None,
        };
        if let Some(l) = local {
            last.status = l.into();
            if l == "complete" || l == "partial" {
                last.last_ok_start = last.start;
            }
            last.end = last.end.max(last.start);
        }
    } else if status == "failure" {
        last.status = "error".into();
        last.last_ok_start = 0;
    }
    let _ = last.save();
}

/// While a backup runs: if the server no longer lists its running task as running (cancelled
/// from the console), stop it.
fn check_cancel(sess: &Session) {
    let id = control::running_task();
    if id == 0 {
        return;
    }
    match sess.connect().and_then(|mut c| sess.running_tasks(&mut c)) {
        Ok(list) => match list.iter().find(|rt| rt.running_task_id == id) {
            Some(rt) if rt.status <= 1 => {}
            Some(rt) => control::cancel(&format!("running task {id} has status {} on the server", rt.status)),
            None => control::cancel(&format!("running task {id} no longer on the server")),
        },
        Err(e) => eprintln!("get_running_task: {e}"),
    }
}

/// Runs the backups the server has queued for this device.
pub fn run_pending(sess: &Session) {
    let pending = sess.connect().and_then(|mut c| sess.running_tasks(&mut c));
    match pending {
        Ok(list) => {
            for rt in list.into_iter().filter(|rt| rt.status == 0) {
                if let Err(e) = backup::run(sess, backup::Trigger::Server(rt)) {
                    eprintln!("backup: {e}");
                }
            }
        }
        Err(e) => eprintln!("get_running_task: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_checked() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let a = Args::parse(&v(&["-a", "nas", "-u", "me"]), &[], &["-a", "-u", "-p", "--otp"], 0).unwrap();
        assert_eq!((a.opt("-a"), a.opt("-u"), a.opt("-p")), (Some("nas"), Some("me"), None));
        assert!(Args::parse(&v(&["--dry-run"]), &["-v", "--verbose"], &[], 0).unwrap_err().contains("--dry-run"));
        assert!(Args::parse(&v(&["-v"]), &["-v", "--verbose"], &[], 0).unwrap().flag("-v"));
        assert!(Args::parse(&v(&["-a"]), &[], &["-a"], 0).unwrap_err().contains("needs a value"));
        assert!(Args::parse(&v(&["sda", "sdb"]), &[], &[], 1).is_err());
        let b = Args::parse(&v(&["sda", "--limit", "4", "sdb"]), &[], &["--limit", "--image"], usize::MAX).unwrap();
        assert_eq!((&b.words, b.opt("--limit")), (&v(&["sda", "sdb"]), Some("4")));
    }

    #[test]
    fn status_formats() {
        assert_eq!(weekdays(&[1, 2, 3, 4, 5]), "Mon–Fri");
        assert_eq!(weekdays(&[0, 1, 2, 3, 4, 5, 6]), "every day");
        assert_eq!(weekdays(&[1, 3, 0]), "Mon, Wed, Sun");
        assert_eq!(weekdays(&[5, 6, 0]), "Fri–Sun");
        assert_eq!(ui::bar(50.0, 4), "██░░");
        assert_eq!(ui::size(512), "512 B");
        assert_eq!(ui::size(3 << 30), "3.0 GiB");
        assert_eq!(ui::fmt_duration(3720), "1 h 02 min");
    }
}
