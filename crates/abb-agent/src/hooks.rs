//! Commands run when a backup starts and ends (`[hooks]` in the configuration), e.g. to ping a
//! monitoring service.

use std::os::unix::process::CommandExt;
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
/// It runs in a process group of its own (out of reach of the terminal's Ctrl-C): on timeout, or
/// when the agent is stopping, everything it started is killed too.
fn exec(cmd: &str, env: &[(&str, String)], timeout: Duration) -> Result<(), String> {
    if cmd.trim().is_empty() {
        return Ok(());
    }
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return Ok(()),
            Ok(Some(s)) => return Err(s.to_string()),
            Ok(None) if start.elapsed() > timeout || crate::control::stopping() => {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                if crate::control::stopping() {
                    return Err("killed: the agent is stopping".into());
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_kills_the_whole_group() {
        let dir = std::env::temp_dir().join(format!("abb-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mark = dir.join("survivor");
        // A child of the shell that would write the mark after the timeout.
        let cmd = format!("(sleep 1; touch {}) & wait", mark.display());
        let t = Instant::now();
        assert!(exec(&cmd, &[], Duration::from_millis(100)).unwrap_err().starts_with("killed"));
        assert!(t.elapsed() < Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!mark.exists(), "a child of the hook survived the timeout");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(exec("exit 3", &[], Duration::from_secs(5)).is_err());
        assert!(exec("true", &[], Duration::from_secs(5)).is_ok());
    }
}
