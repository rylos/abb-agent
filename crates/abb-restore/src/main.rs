//! abb-restore: bare-metal restore from Synology Active Backup for Business. One static
//! binary to run from any Linux live system: a guided text interface, or commands for scripts.

mod btrfs;
mod fs;
mod gpt;
mod local;
mod plan;
mod restore;
mod server;
mod spec;
mod tui;

use abb_proto::transport::{DEFAULT_PORT, fingerprint_hex};
use local::size_str;
use plan::{Mode, Place, Plan, VolumeTarget};
use server::Server;
use spec::{Task, Version};
use std::io::Write;
use std::sync::atomic::AtomicBool;

const USAGE: &str = "usage:
  abb-restore                                    guided restore (text interface)
  abb-restore disks                              local disks
  abb-restore list     -a <server> -u <user>     devices and tasks on the server
  abb-restore versions -a <server> -u <user> -t <task>
                                                 versions of a task, with their disks and volumes
  abb-restore run      -a <server> -u <user> -t <task> [-v <version>] <mode> [--grow]
                       [--no-efi-fallback] --yes
     mode: --device [--map <disk>=<local disk>]…   every disk (default mapping: automatic)
           --system [--map <disk>=<local disk>]…   only the volumes needed to boot, in place
           --volume <volume>=<local disk>:free|<local partition number>…
     --exclude-subvolume <pattern>…   leave btrfs subvolumes out (e.g. @games): their data is not
                                      downloaded, their snapshots go with them, they come back empty
  options: -p <port>, --otp <code>; the password comes from ABB_PASSWORD or is asked for.

Disks of the backup are named as on the original machine (sda, nvme0n1); volumes by mount path
or device name (/dev/sda2); local disks by kernel name (sdb, nvme0n1).";

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("abb-restore: {msg}");
    std::process::exit(1)
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

pub fn prompt_password(user: &str) -> String {
    eprint!("password for {user}: ");
    let _ = std::io::stderr().flush();
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    let tty = unsafe { libc::tcgetattr(0, &mut t) } == 0;
    if tty {
        let mut quiet = t;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) };
    }
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    if tty {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        eprintln!();
    }
    s.trim_end_matches(['\n', '\r']).to_string()
}

fn date(t: u64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = t as i64;
    unsafe { libc::localtime_r(&t as *const i64 as *const _, &mut tm) };
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opt = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let opts = |flag: &str| -> Vec<String> {
        args.windows(2).filter(|w| w[0] == flag).map(|w| w[1].clone()).collect()
    };
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let connect = || {
        let host = opt("-a").unwrap_or_else(|| usage());
        let user = opt("-u").unwrap_or_else(|| usage());
        let port = opt("-p").map_or(DEFAULT_PORT, |p| p.parse().unwrap_or_else(|_| usage()));
        let password = std::env::var("ABB_PASSWORD").unwrap_or_else(|_| prompt_password(&user));
        let mut s = Server::new(&host, port, &user, &password, &opt("--otp").unwrap_or_default());
        let version = s.login().unwrap_or_else(|e| die(e));
        eprintln!("{host}: Active Backup for Business {version}, certificate {}", fingerprint_hex(&s.pin.unwrap()));
        s
    };
    let find_task = |s: &mut Server| {
        let id: u64 = opt("-t").and_then(|t| t.parse().ok()).unwrap_or_else(|| usage());
        let tasks = s.tasks().unwrap_or_else(|e| die(e));
        tasks.iter().map(Task::from).find(|t| t.task_id == id).unwrap_or_else(|| die(format!("no task {id}")))
    };
    match args.get(1).map(String::as_str) {
        None => tui::run(),
        Some("disks") => {
            for d in local::disks() {
                println!(
                    "{:<10} {:>10}  sector {:<4} {}{}{}",
                    d.name,
                    size_str(d.size),
                    d.sector_size,
                    d.model,
                    if d.removable { "  removable" } else { "" },
                    d.busy.as_ref().map_or(String::new(), |b| format!("  [in use: {b}]"))
                );
                for p in &d.partitions {
                    println!("  {:<3} {:<12} {:>10} at {}", p.number, p.name, size_str(p.length), size_str(p.offset));
                }
                if let Some(g) = &d.gpt {
                    for (start, end) in g.free(d.size) {
                        println!("  free         {:>10} at sector {start}", size_str((end - start + 1) * d.sector_size));
                    }
                }
            }
        }
        Some("list") => {
            let mut s = connect();
            for t in s.tasks().unwrap_or_else(|e| die(e)).iter().map(Task::from) {
                let latest = if t.latest == 0 { "no version".into() } else { date(t.latest) };
                println!("task {:<4} {:<24} {:<24} {:<32} latest {latest}", t.task_id, t.host_name, t.task_name, t.os_name);
            }
        }
        Some("versions") => {
            let mut s = connect();
            let t = find_task(&mut s);
            let resp = s.versions(t.device_id, t.task_id).unwrap_or_else(|e| die(e));
            for v in Version::list(&resp) {
                println!("version {:<5} {}{}", v.id, date(v.time), if v.bootable { "" } else { "  (not bootable)" });
                for d in &v.disks {
                    println!("  disk {:<8} {:>10} {}", d.name(), size_str(d.size), if d.gpt { "GPT" } else { "MBR" });
                    for p in &d.partitions {
                        println!(
                            "    {:<3} {:<20} {:>10} {:<6}{}{}",
                            p.number,
                            p.label,
                            size_str(p.length),
                            p.fs,
                            if p.is_system() { " system" } else { "" },
                            if p.excluded { " (not backed up)" } else { "" }
                        );
                    }
                }
            }
        }
        Some("run") => {
            let mode = match (has("--device"), has("--system"), !opts("--volume").is_empty()) {
                (true, false, false) => Mode::Device,
                (false, true, false) => Mode::System,
                (false, false, true) => Mode::Volumes,
                _ => usage(),
            };
            let locals = local::disks();
            let local_index = |name: &str| {
                let name = name.trim_start_matches("/dev/");
                locals.iter().position(|l| l.name == name).unwrap_or_else(|| die(format!("no local disk {name}")))
            };
            let mut s = connect();
            let t = find_task(&mut s);
            let resp = s.versions(t.device_id, t.task_id).unwrap_or_else(|e| die(e));
            let versions = Version::list(&resp);
            let v = match opt("-v") {
                Some(id) => versions.iter().find(|v| v.id.to_string() == id).unwrap_or_else(|| die(format!("no version {id}"))),
                None => versions.first().unwrap_or_else(|| die("the task has no version")),
            };
            let disk_index = |name: &str| v.disks.iter().position(|d| d.name() == name).unwrap_or_else(|| die(format!("no disk {name} in the backup")));
            let mut plan = Plan { mode, disks: vec![None; v.disks.len()], volumes: vec![], grow: has("--grow"), efi_fallback: !has("--no-efi-fallback"), exclude_subvolumes: opts("--exclude-subvolume") };
            match mode {
                Mode::Device | Mode::System => {
                    let maps = opts("--map");
                    if maps.is_empty() {
                        plan.disks = plan::auto_map(v, &locals);
                    }
                    for m in maps {
                        let (a, b) = m.split_once('=').unwrap_or_else(|| usage());
                        plan.disks[disk_index(a)] = Some(local_index(b));
                    }
                }
                Mode::Volumes => {
                    for spec in opts("--volume") {
                        let (vol, target) = spec.rsplit_once('=').unwrap_or_else(|| usage());
                        let (disk, part) = v
                            .disks
                            .iter()
                            .enumerate()
                            .find_map(|(i, d)| {
                                d.partitions.iter().position(|p| [&p.label, &p.mount_path, &p.device].iter().any(|n| n.as_str() == vol)).map(|j| (i, j))
                            })
                            .unwrap_or_else(|| die(format!("no volume {vol} in the backup")));
                        let (ldisk, place) = target.split_once(':').unwrap_or_else(|| usage());
                        let li = local_index(ldisk);
                        let place = if place == "free" {
                            plan::first_fit(v, &locals, li, disk, part, &plan.volumes)
                                .map(Place::Free)
                                .unwrap_or_else(|| die(format!("no free space for {vol} on {ldisk}")))
                        } else {
                            Place::Partition(place.parse().unwrap_or_else(|_| usage()))
                        };
                        plan.volumes.push(VolumeTarget { disk, part, local: li, place });
                    }
                }
            }
            let errs = plan.check(v, &locals);
            if !errs.is_empty() {
                die(errs.join("\n"));
            }
            println!("restore {} version {} ({}) of {}, mode: {}", t.task_name, v.id, date(v.time), t.host_name, mode.title());
            for (i, m) in plan.disks.iter().enumerate() {
                if let Some(j) = m {
                    println!("  disk {} ({}) → /dev/{} ({})", v.disks[i].name(), size_str(v.disks[i].size), locals[*j].name, size_str(locals[*j].size));
                }
            }
            for o in &plan.volumes {
                println!("  {} → /dev/{} {:?}", v.disks[o.disk].partitions[o.part].label, locals[o.local].name, o.place);
            }
            if !has("--yes") {
                die("everything on the targets above will be overwritten: add --yes to go ahead");
            }
            let cancel = AtomicBool::new(false);
            let mut last = 0;
            let res = restore::run(&mut s, &t, v, &plan, &locals, &cancel, &mut |e| match e {
                restore::Event::Step(m) => eprintln!("{m}"),
                restore::Event::Warning(m) => eprintln!("warning: {m}"),
                restore::Event::Progress { done, total } => {
                    let pct = done * 100 / total.max(1);
                    // Every step (superblock, metadata, data…) counts from zero again.
                    if pct < last {
                        last = 0;
                    }
                    if pct >= last + 5 || done == total {
                        last = pct;
                        eprintln!("  {pct:>3}%  {} of {}", size_str(done), size_str(total));
                    }
                }
            });
            match res {
                Ok(()) => println!("restore complete"),
                Err(e) => die(e),
            }
        }
        Some("version" | "--version" | "-V") => println!("abb-restore {}", env!("CARGO_PKG_VERSION")),
        _ => usage(),
    }
}
