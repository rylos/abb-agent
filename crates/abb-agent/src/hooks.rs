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
    if let Err(e) = exec(cmd, env, TIMEOUT) {
        eprintln!("{name}: {e}");
    }
}

/// Runs `cmd` with /bin/sh; Ok if it exits with 0 within `timeout` (an empty command is Ok).
fn exec(cmd: &str, env: &[(&str, String)], timeout: Duration) -> Result<(), String> {
    if cmd.trim().is_empty() {
        return Ok(());
    }
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return Ok(()),
            Ok(Some(s)) => return Err(s.to_string()),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("killed after {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// `schedule.only_if`: whether the agent may start a scheduled backup now.
pub fn allowed() -> bool {
    exec(&crate::config::get().schedule.only_if, &[], Duration::from_secs(30)).is_ok()
}

pub fn on_start() {
    run("on_start", &crate::config::get().hooks.on_start, &[]);
}

/// `status`: "complete", "partial", "error" or "cancel".
pub fn on_finish(status: &str) {
    let env = [("ABB_STATUS", status.to_string()), ("ABB_EXIT", exit_code(status).to_string())];
    run("on_finish", &crate::config::get().hooks.on_finish, &env);
}
