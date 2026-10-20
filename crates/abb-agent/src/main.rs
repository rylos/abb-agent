//! abb-agent: Linux client for Synology Active Backup for Business, with image backup
//! of btrfs volumes.

mod backup;
mod config;
mod control;
mod disk;
mod host;
mod nodes;
mod scheduler;
mod scripts;
mod session;
mod stream;
mod task;
mod volume;
mod state;

use abb_proto::transport::{DEFAULT_PORT, fingerprint_hex};
use abb_proto::{Connection, Value, request};
use session::Session;
use state::State;
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  abb-agent connect -a <server> -u <user> [-p <port>]    pair this computer with the ABB server
                                                         (password from ABB_PASSWORD or prompted)
  abb-agent status                                       connection and task status
  abb-agent daemon                                       service: heartbeat, events, scheduled backups
  abb-agent backup                                       back up now, in the foreground
  abb-agent config                                       configuration in effect (/etc/abb-agent.toml)
  abb-agent disks [disk]                                 disks, partitions and filesystems
  abb-agent bench [disk…] [--freeze-ext] [--limit GiB]   offline test: snapshot, read,
                                                         verify and compress the whole disk
  abb-agent probe <server>                               server version, without pairing
  abb-agent version                                      agent version";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opt = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let cfg = config::load().unwrap_or_else(|e| die(format!("configuration: {e}")));
    match args.get(1).map(String::as_str) {
        Some("config") => {
            println!("# {}", config::path());
            print!("{}", toml::to_string_pretty(cfg).unwrap_or_else(|e| die(e)));
        }
        Some("version" | "--version" | "-V") => println!("abb-agent {}", env!("CARGO_PKG_VERSION")),
        Some("connect") => {
            let server = opt("-a").unwrap_or_else(|| usage());
            let user = opt("-u").unwrap_or_else(|| usage());
            let port = opt("-p").map_or(DEFAULT_PORT, |p| p.parse().unwrap_or_else(|_| usage()));
            let password = std::env::var("ABB_PASSWORD").unwrap_or_else(|_| prompt_password(&user));
            let state = session::pair(&server, port, &user, &password, None).unwrap_or_else(|e| die(e));
            state.save().unwrap_or_else(|e| die(e));
            println!("paired with {server} as {user}: device and task created on the server");
        }
        Some("status") => status(),
        Some("backup") => {
            let sess = load();
            sess.auth_by_token(false).unwrap_or_else(|e| die(e));
            let trigger = backup::Trigger::Local { trigger_type: backup::TRIGGER_MANUAL, trigger_event: None };
            backup::run(&sess, trigger).unwrap_or_else(|e| die(e));
        }
        Some("daemon") => daemon(),
        Some("disks") => {
            let name = args.get(2).cloned().unwrap_or_else(|| disk::system_disk().unwrap_or_else(|e| die(e)));
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
        Some("bench") => {
            let names = args[2..].to_vec();
            let freeze = cfg.ext.freeze_mounted || args.iter().any(|a| a == "--freeze-ext");
            let limit = opt("--limit").map(|g| g.parse::<u64>().unwrap_or_else(|_| usage()) << 30);
            let names: Vec<String> = names
                .into_iter()
                .filter(|a| !a.starts_with("--") && Some(a) != opt("--limit").as_ref())
                .collect();
            let names = if names.is_empty() { vec![disk::system_disk().unwrap_or_else(|e| die(e))] } else { names };
            backup::bench(names, freeze, limit).unwrap_or_else(|e| die(e));
        }
        Some("probe") => {
            let host = args.get(2).cloned().unwrap_or_else(|| usage());
            let mut c = Connection::open(&host, DEFAULT_PORT, None).unwrap_or_else(|e| die(e));
            println!("certificate: {}", fingerprint_hex(&c.fingerprint));
            let r = c.call(&request("query_server_info", vec![])).unwrap_or_else(|e| die(e));
            println!("{r:#?}");
        }
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

fn die(e: impl std::fmt::Display) -> ! {
    eprintln!("error: {e}");
    std::process::exit(1);
}

fn prompt_password(user: &str) -> String {
    eprint!("password for {user}: ");
    let _ = std::io::stderr().flush();
    let fd = 0;
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    let tty = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
    if tty {
        let mut quiet = term;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    }
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if tty {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
        eprintln!();
    }
    line.trim_end_matches(['\n', '\r']).to_string()
}

fn load() -> Session {
    let state = State::load().unwrap_or_else(|e| die(format!("cannot read state ({e}): run `abb-agent connect` first")));
    Session::new(state)
}

fn status() {
    let sess = load();
    println!("server {}:{}  user {}  device {}", sess.state.server, sess.state.port, sess.state.username, sess.state.device_uuid);
    sess.auth_by_token(false).unwrap_or_else(|e| die(e));
    let mut c = sess.connect().unwrap_or_else(|e| die(e));
    let tasks = sess.tasks(&mut c).unwrap_or_else(|e| die(e));
    for t in &tasks {
        println!(
            "task {} {:?}",
            t.get("id").and_then(|v| v.as_int()).unwrap_or(0),
            t.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        );
        if let Some(info) = task::TaskInfo::parse(t) {
            println!("  source: {:?}", info.selection);
            let sch = &info.schedule;
            if sch.is_scheduled() {
                println!(
                    "  console schedule: days {:?} (0 = Sunday) at {:02}:{:02} server time{}{}",
                    sch.weekdays,
                    sch.hour,
                    sch.minute,
                    if sch.hourly { ", then every hour" } else { "" },
                    if sch.window.is_some() { ", within the backup window" } else { "" }
                );
            } else {
                println!("  console schedule: manual only");
            }
        }
    }
    for rt in sess.running_tasks(&mut c).unwrap_or_else(|e| die(e)) {
        println!("queued/running: {rt:?}");
    }
    let last = state::LastRun::load();
    let sc = &config::get().schedule;
    println!(
        "local schedule: after boot {}, every {} h, retry after {} min",
        if sc.on_boot { "yes" } else { "no" },
        if sc.interval_hours == 0 { "-".into() } else { sc.interval_hours.to_string() },
        sc.retry_minutes
    );
    println!(
        "last backup: start {} end {} result {:?}{}",
        fmt_time(last.start),
        fmt_time(last.end),
        last.status,
        if last.server_status.is_empty() { String::new() } else { format!(", on the server {:?}", last.server_status) }
    );
    if let Ok(off) = sess.server_offset(&mut c) {
        control::set_server_offset(off);
    }
    let console = tasks.first().and_then(task::TaskInfo::parse).and_then(|t| scheduler::console_due(&t));
    let local = scheduler::local_due(sc, &last).map(|d| d.0.max(host::now()));
    match [console, local].into_iter().flatten().min() {
        Some(t) => println!("next backup: {}", fmt_time(t)),
        None => println!("next backup: when started from the console or with `abb-agent backup`"),
    }
}

fn fmt_time(t: u64) -> String {
    if t == 0 {
        return "-".into();
    }
    let tt = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Service: heartbeat, schedule and backups each in a thread of their own; the main thread
/// handles the server's events, also while a backup runs (cancel, bandwidth, console requests).
fn daemon() {
    let sess = load();
    backup::startup_cleanup();
    loop {
        match sess.auth_by_token(true) {
            Ok(()) => break,
            Err(e) => {
                eprintln!("auth: {e}; retrying in 30 s");
                std::thread::sleep(Duration::from_secs(30));
            }
        }
    }
    eprintln!("connected to {} as {}", sess.state.server, sess.host.host_name);

    let hb_state = sess.state.clone();
    std::thread::spawn(move || {
        let s = Session::new(hb_state);
        let mut conn: Option<Connection> = None;
        let mut last_auth = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(6));
            if conn.is_none() {
                conn = s.connect().map_err(|e| eprintln!("heartbeat: {e}")).ok();
            }
            if let Some(c) = conn.as_mut()
                && let Err(e) = s.heart_beat(c)
            {
                eprintln!("heartbeat: {e}");
                conn = None;
            }
            if last_auth.elapsed() > Duration::from_secs(60) {
                if let Err(e) = s.auth_by_token(false) {
                    eprintln!("auth renewal: {e}");
                }
                last_auth = Instant::now();
            }
        }
    });

    // Backups run one at a time in the worker: local triggers come from the scheduler, `None`
    // means "run what the server has queued".
    let (tx, rx) = std::sync::mpsc::channel::<Option<scheduler::LocalTrigger>>();
    let worker_state = sess.state.clone();
    std::thread::spawn(move || {
        let s = Session::new(worker_state);
        for req in rx {
            control::set_busy(true);
            if let Some((trigger_type, trigger_event)) = req {
                let last = state::LastRun::load();
                if let Err(e) = backup::run(&s, backup::Trigger::Local { trigger_type, trigger_event }) {
                    eprintln!("backup: {e}");
                    // Failed before starting (e.g. server unreachable): retry later.
                    let mut l = state::LastRun::load();
                    if l.start == last.start {
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
    std::thread::spawn(move || {
        let (stx, srx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for t in srx {
                let _ = sched_tx.send(Some(t));
            }
        });
        scheduler::run(Session::new(sched_state), stx)
    });

    let mut sess = sess;
    let mut poll: Option<Connection> = None;
    // A backup may already be queued at startup.
    let _ = tx.send(None);
    loop {
        if poll.is_none() {
            match sess.connect() {
                Ok(c) => {
                    let _ = c.set_read_timeout(Some(Duration::from_secs(450)));
                    poll = Some(c);
                }
                Err(e) => {
                    eprintln!("long_poll: {e}");
                    std::thread::sleep(Duration::from_secs(15));
                    continue;
                }
            }
        }
        let c = poll.as_mut().unwrap();
        let has = match sess.long_poll(c, 400) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("long_poll: {e}");
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
                "update_running_task" => check_tasks = true,
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
                _ => {}
            }
        }
        for ev in &events {
            if let Err(e) = sess.done_event(c, ev.id) {
                eprintln!("done_event {}: {e}", ev.id);
            }
            sess.state.last_event_id = sess.state.last_event_id.max(ev.id);
        }
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
    if status.is_empty() || status == last.server_status {
        return;
    }
    eprintln!("result {} on the server: {status}", last.result_id);
    last.server_status = status.clone();
    if status == "failure" && last.status != "running" {
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
