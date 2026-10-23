//! Versions: the Active Backup package on the NAS, the official agent it offers, whether it
//! accepts this agent, and the console's "Update agent" request.
//!
//! The server checks only `agent.version.build` (`VersionCheckerMiddleware`): its last four
//! digits must be above 4400 for most actions and above 4496 for a few (image restore, task
//! versions); a missing or non-numeric build counts as 0 and the server then leaves requests
//! unanswered. It never refuses a newer build, and an agent that is not updated keeps backing
//! up. abb-agent announces the official agent's 3.2.2 build 5554 and keeps it fixed.

use crate::session::Session;
use crate::state::STATE_DIR;
use abb_proto::Value;
use abb_proto::transport::{AGENT_BUILD, Error};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `SPROTO_ERR_USER_TOKEN_NOT_FOUND`: the device was removed on the server.
pub const ERR_TOKEN_NOT_FOUND: u64 = 0x100A;
/// "Client version too low" (`VersionCheckerMiddleware`).
pub const ERR_CLIENT_TOO_OLD: u64 = 0x4001;
/// The official agent refuses a server whose build (last four digits) is not above this.
const MIN_SERVER_BUILD: u64 = 4400;

/// The Active Backup package on the NAS (`query_server_info`): its build less 20000 is the
/// build of the official agent it offers (25059 → 5059).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Package {
    pub major: u64,
    pub minor: u64,
    pub mini: u64,
    pub build: u64,
}

impl Package {
    /// Build of the official agent that goes with it, as the official agent compares builds.
    pub fn agent_build(&self) -> u64 {
        self.build % 10000
    }
}

impl std::fmt::Display for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}-{}", self.major, self.minor, self.mini, self.build)
    }
}

/// Build this agent announces.
pub fn ours() -> u64 {
    AGENT_BUILD.parse().unwrap_or(0)
}

pub fn query(sess: &Session) -> Result<Package, String> {
    let r = sess.connect().and_then(|mut c| sess.call(&mut c, "query_server_info", vec![])).map_err(|e| e.to_string())?;
    let v = r.get("package_version").ok_or("no package_version in the reply")?;
    let num = |k: &str| match v.get(k) {
        Some(x) => x.as_int().or_else(|| x.as_str().and_then(|s| s.trim().parse().ok())).unwrap_or(0),
        None => 0,
    };
    Ok(Package { major: num("major"), minor: num("minor"), mini: num("mini"), build: num("build") })
}

/// What the agent knows about the NAS between runs (`server.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerInfo {
    /// Active Backup package on the NAS, e.g. "3.2.1-25059".
    pub package: String,
    /// When the NAS started refusing this agent (Unix seconds, 0 = it accepts it), and why.
    pub refused_since: u64,
    pub refused: String,
    /// Official agent already reported to the console as not installed (log 7016).
    pub update_reported: String,
}

impl ServerInfo {
    fn path() -> PathBuf {
        Path::new(STATE_DIR).join("server.json")
    }

    pub fn load() -> ServerInfo {
        std::fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(&self) {
        let tmp = Self::path().with_extension("tmp");
        let r = std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default()).and_then(|_| std::fs::rename(&tmp, Self::path()));
        if let Err(e) = r {
            eprintln!("saving {}: {e}", Self::path().display());
        }
    }
}

/// Records the package on the NAS and says in the journal when it changed and what that means
/// for this agent. At startup and after the server comes back (a package update restarts it).
pub fn check_server(sess: &Session) {
    let p = match query(sess) {
        Ok(p) => p,
        Err(e) => return eprintln!("query_server_info: {e}"),
    };
    let mut info = ServerInfo::load();
    let now = p.to_string();
    if info.package == now {
        return;
    }
    if info.package.is_empty() {
        eprintln!("Active Backup on the NAS: {now}");
    } else {
        eprintln!("Active Backup on the NAS changed from {} to {now}", info.package);
    }
    for w in warnings(&p) {
        eprintln!("{w}");
    }
    info.package = now;
    info.save();
}

/// What a package on the NAS means for this agent, for the journal and `status`.
pub fn warnings(p: &Package) -> Vec<String> {
    let mut w = Vec::new();
    if p.agent_build() <= MIN_SERVER_BUILD {
        w.push(format!("Active Backup {p} on the NAS is older than the official agent 3.2.2 accepts: update the package in DSM"));
    }
    if p.agent_build() > ours() {
        w.push(format!(
            "the NAS offers the official agent build {} (abb-agent announces {}): the console may show an update; update abb-agent with the package manager instead",
            p.agent_build(),
            ours()
        ));
    }
    w
}

/// Why the NAS refuses this agent, if `e` is such a refusal: retrying soon does not help.
pub fn refusal(e: &Error) -> Option<String> {
    match e {
        Error::Server { code: ERR_CLIENT_TOO_OLD, .. } => Some(format!(
            "the NAS refuses this agent as too old (client version too low for build {}): update abb-agent",
            ours()
        )),
        Error::Server { code: ERR_TOKEN_NOT_FOUND, .. } => {
            Some("the NAS no longer knows this device (removed from the console?): pair it again with `sudo abb-agent connect`".into())
        }
        _ => None,
    }
}

/// Notes a refusal; true the first time (to log it once).
pub fn refused(why: &str) -> bool {
    let mut info = ServerInfo::load();
    if info.refused_since != 0 && info.refused == why {
        return false;
    }
    info.refused_since = crate::host::now();
    info.refused = why.to_string();
    info.save();
    true
}

/// The NAS accepted a request: clears a refusal noted before.
pub fn accepted() {
    let mut info = ServerInfo::load();
    if info.refused_since == 0 {
        return;
    }
    eprintln!("the NAS accepts this agent again");
    info.refused_since = 0;
    info.refused.clear();
    info.save();
}

/// "Update agent" from the ABB console. The official agent downloads Synology's package and
/// installs it; this one never does (it would replace abb-agent): it tells the journal, and the
/// console once per offered version (log 7016, "the agent could not be updated"), then always
/// closes the request with `finish_update`, which is what clears the "waiting for update" state
/// on the server. It never asks for the installer (`start_download`).
pub fn update_requested(sess: &Session) {
    match query(sess) {
        Ok(p) if p.agent_build() > ours() => {
            eprintln!(
                "update requested from the ABB console (the NAS offers the official agent build {}): abb-agent is not updated this way, update it with the package manager of the distribution",
                p.agent_build()
            );
            let mut info = ServerInfo::load();
            if info.update_reported != p.to_string() {
                let entry = Value::map([
                    ("log_time", Value::Int(crate::host::now())),
                    ("log_type", Value::Int(7016)),
                    ("other_params", Value::str("null\n")),
                ]);
                let r = sess.connect().and_then(|mut c| {
                    sess.call(&mut c, "log_report", vec![("subaction", Value::str("activity_log")), ("logs", Value::Array(vec![entry]))])
                });
                match r {
                    Ok(_) => {
                        info.update_reported = p.to_string();
                        info.save();
                    }
                    Err(e) => eprintln!("log_report 7016: {e}"),
                }
            }
        }
        Ok(_) => eprintln!("update requested from the ABB console: abb-agent is as new as the agent the NAS offers"),
        Err(e) => eprintln!("query_server_info: {e}"),
    }
    let r = sess.connect().and_then(|mut c| sess.call(&mut c, "download_installer", vec![("subaction", Value::str("finish_update"))]));
    if let Err(e) = r {
        eprintln!("finish_update: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announced_build_passes_every_server_check() {
        // A non-numeric build counts as 0 on the server and leaves requests unanswered; the
        // server compares the last four digits with 4400 and, for restores, 4496.
        assert_eq!(AGENT_BUILD.len(), 4);
        let b: u64 = AGENT_BUILD.parse().unwrap();
        assert!(b % 10000 > 4496);
    }

    #[test]
    fn warnings_for_old_and_newer_servers() {
        let p = |build| Package { major: 3, minor: 2, mini: 1, build };
        assert!(warnings(&p(25059)).is_empty());
        assert!(warnings(&p(24400))[0].contains("older"));
        assert!(warnings(&p(25600))[0].contains("offers the official agent build 5600"));
    }
}
