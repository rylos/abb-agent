//! Pre/post snapshot scripts set in the ABB console. The server sends their content with the
//! task; they run as root with `/bin/sh` in a directory of their own, with a 180 s limit. The
//! pre script runs before the snapshots, the post script right after them (e.g. to pause and
//! resume a database), and their output goes to the server.

use crate::backup::Job;
use crate::state::STATE_DIR;
use crate::task::{ScriptMode, Scripts};
use abb_proto::transport::{Error, Result};
use abb_proto::{Connection, Value};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    Pre,
    Post,
}

impl Hook {
    fn name(self) -> &'static str {
        match self {
            Hook::Pre => "pre",
            Hook::Post => "post",
        }
    }

    /// Log codes: success, failure ignored, failure stopping the backup, timeout.
    fn codes(self) -> [u64; 4] {
        match self {
            Hook::Pre => [7045, 7046, 7047, 7048],
            Hook::Post => [7049, 7050, 7051, 7052],
        }
    }
}

enum Outcome {
    Ok,
    Failed(String),
    Timeout,
}

fn execute(dir: &Path, script: &Path) -> std::io::Result<(Outcome, Vec<u8>)> {
    let log = dir.join("output.log");
    let out = std::fs::File::create(&log)?;
    let mut child = Command::new("/bin/sh")
        .arg(script)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .process_group(0)
        .spawn()?;
    let start = Instant::now();
    let outcome = loop {
        if let Some(st) = child.try_wait()? {
            break if st.success() { Outcome::Ok } else { Outcome::Failed(st.to_string()) };
        }
        if start.elapsed() > TIMEOUT {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            break Outcome::Timeout;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    Ok((outcome, std::fs::read(&log).unwrap_or_default()))
}

/// Runs the `hook` script of the task, if enabled, and reports it. Fails if the script failed
/// and the task requires success.
pub fn run(job: &Job, c: &mut Connection, scripts: &Scripts, hook: Hook, version_id: u64) -> Result<()> {
    let entry = match hook {
        Hook::Pre => &scripts.pre,
        Hook::Post => &scripts.post,
    };
    let Some(entry) = entry else { return Ok(()) };
    let required = scripts.mode == ScriptMode::RequireSuccess;
    let [ok, ignored, stopped, timeout] = hook.codes();
    let content = match entry {
        Ok(c) => c,
        Err(code) => {
            // The server could not read the script (e.g. 7054: file not found on the NAS).
            let code = if *code == 0 { 7053 } else { *code };
            job.log(c, code, None);
            return if required {
                Err(Error::Protocol(format!("{} script not available (code {code})", hook.name())))
            } else {
                Ok(())
            };
        }
    };
    let dir = Path::new(STATE_DIR).join("scripts").join(&job.rt.task_uuid);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let script = dir.join(format!("{}.sh", hook.name()));
    std::fs::write(&script, content)?;
    eprintln!("running the {} script", hook.name());
    let (outcome, output) = execute(&dir, &script)?;
    let failure = match &outcome {
        Outcome::Ok => None,
        Outcome::Failed(s) => Some(format!("{} script failed: {s}", hook.name())),
        Outcome::Timeout => Some(format!("{} script killed after {} s", hook.name(), TIMEOUT.as_secs())),
    };
    let code = match (&outcome, required) {
        (Outcome::Ok, _) => ok,
        (Outcome::Timeout, _) => timeout,
        (Outcome::Failed(_), true) => stopped,
        (Outcome::Failed(_), false) => ignored,
    };
    let entry = Value::map([
        ("output", Value::Buffer(output)),
        ("output_source", Value::str(hook.name())),
        ("task_id", Value::Int(job.rt.task_id)),
        ("version_id", Value::Int(version_id)),
    ]);
    if let Err(e) = job.sess.call(c, "log_report", vec![("subaction", Value::str("script_log")), ("logs", Value::Array(vec![entry]))]) {
        eprintln!("script_log: {e}");
    }
    job.log(c, code, None);
    match failure {
        Some(f) if required => Err(Error::Protocol(f)),
        Some(f) => {
            eprintln!("{f} (ignored)");
            Ok(())
        }
        None => Ok(()),
    }
}
