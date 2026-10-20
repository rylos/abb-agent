//! Commands run when a backup starts and ends (`[hooks]` in the configuration), e.g. to ping a
//! monitoring service.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Longest a hook may run before it is killed.
const TIMEOUT: Duration = Duration::from_secs(300);

/// `ABB_EXIT` for a final status: 0 only for a complete backup.
fn exit_code(status: &str) -> u8 {
    match status {
        "complete" => 0,
        "partial" => 1,
        "cancel" => 3,
        _ => 2,
    }
}

fn run(name: &str, cmd: &str, env: &[(&str, String)]) {
    if cmd.trim().is_empty() {
        return;
    }
    let mut child = match Command::new("/bin/sh").arg("-c").arg(cmd).envs(env.iter().map(|(k, v)| (k, v))).stdin(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{name}: {e}");
            return;
        }
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return,
            Ok(Some(s)) => {
                eprintln!("{name}: {s}");
                return;
            }
            Ok(None) if start.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("{name}: killed after {} s", TIMEOUT.as_secs());
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => {
                eprintln!("{name}: {e}");
                return;
            }
        }
    }
}

pub fn on_start() {
    run("on_start", &crate::config::get().hooks.on_start, &[]);
}

/// `status`: "complete", "partial", "error" or "cancel".
pub fn on_finish(status: &str) {
    let env = [("ABB_STATUS", status.to_string()), ("ABB_EXIT", exit_code(status).to_string())];
    run("on_finish", &crate::config::get().hooks.on_finish, &env);
}
