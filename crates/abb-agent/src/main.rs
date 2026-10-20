//! abb-agent: Linux client for Synology Active Backup for Business, with image backup
//! of btrfs volumes.

mod backup;
mod config;
mod disk;
mod host;
mod scheduler;
mod session;
mod stream;
mod volume;
mod state;

use abb_proto::transport::{DEFAULT_PORT, fingerprint_hex};
use abb_proto::{Connection, request};
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
            backup::run(&sess, backup::Trigger::Local { trigger_type: backup::TRIGGER_MANUAL }).unwrap_or_else(|e| die(e));
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
    for t in sess.tasks(&mut c).unwrap_or_else(|e| die(e)) {
        println!(
            "task {} {:?}  compression {:?}  encryption {:?}",
            t.get("id").and_then(|v| v.as_int()).unwrap_or(0),
            t.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            t.get("compression").and_then(|v| v.as_int()),
            t.get("encryption").and_then(|v| v.as_int()),
        );
    }
    for rt in sess.running_tasks(&mut c).unwrap_or_else(|e| die(e)) {
        println!("queued/running: {rt:?}");
    }
    let last = state::LastRun::load();
    let sc = &config::get().schedule;
    println!(
        "schedule: at boot {}, every {} h; last backup: start {} end {} result {:?}",
        if sc.on_boot { "yes" } else { "no" },
        sc.interval_hours,
        fmt_time(last.start),
        fmt_time(last.end),
        last.status
    );
    if let Some(t) = scheduler::next_due(sc, &last) {
        println!("next backup: {}", fmt_time(t));
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

/// Service: heartbeat in one thread, events and backups in the main thread.
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

    let sched_state = sess.state.clone();
    std::thread::spawn(move || scheduler::run(Session::new(sched_state)));

    let mut sess = sess;
    let mut poll: Option<Connection> = None;
    // A backup may already be queued at startup.
    run_pending(&sess);
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
        for (id, action, task_id) in &events {
            eprintln!("event {id}: {action} (task {task_id})");
            if action == "update_running_task" || action == "change_task" || action == "update_task" {
                check_tasks = true;
            }
        }
        for (id, _, _) in &events {
            if let Err(e) = sess.done_event(c, *id) {
                eprintln!("done_event {id}: {e}");
            }
            sess.state.last_event_id = sess.state.last_event_id.max(*id);
        }
        let _ = sess.state.save();
        if check_tasks {
            run_pending(&sess);
        }
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
