//! Answers to `act_client` events: requests the ABB console sends to the agent while someone is
//! looking at it. `list_nodes` with an empty `node_path` lists the volumes for the "custom
//! volume" picker; with a path it lists that directory.

use crate::disk::{self, Fs};
use crate::session::{Event, Session};
use abb_proto::Value;
use std::ffi::CString;

fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let f = st.f_frsize as u64;
    Some((st.f_blocks as u64 * f, st.f_bavail as u64 * f))
}

/// Name of the filesystem as the console expects it. The console only lets Linux users pick
/// ext2/3/4 and XFS volumes, so btrfs is presented as ext4 here (and only here): the backup
/// itself describes it as btrfs.
fn picker_fs(fs: &Fs) -> &'static str {
    match fs {
        Fs::Btrfs | Fs::Ext(4) => "ext4",
        Fs::Ext(3) => "ext3",
        Fs::Ext(_) => "ext2",
        Fs::Xfs => "xfs",
        Fs::Vfat => "vfat",
        _ => "",
    }
}

/// Mounted volumes of the internal disks: one entry per filesystem, under its main mount path.
fn volumes() -> Vec<Value> {
    let mut out = Vec::new();
    for name in disk::internal_disks().unwrap_or_default() {
        let Ok(d) = disk::scan(&name) else { continue };
        for p in d.partitions.iter().filter(|p| !p.mount_path.is_empty() && !picker_fs(&p.fs).is_empty()) {
            let (total, free) = statvfs(&p.mount_path).unwrap_or((0, 0));
            out.push(Value::map([
                ("file_system", Value::str(picker_fs(&p.fs))),
                ("free_size", Value::Int(free)),
                ("is_dir", Value::Int(1)),
                ("is_dynamic_volume", Value::Int(0)),
                ("is_system_drive", Value::Int((p.mount_path == "/") as u64)),
                ("name", Value::str(p.mount_path.clone())),
                ("path", Value::str(p.mount_path.clone())),
                ("total_size", Value::Int(total)),
                ("used_size", Value::Int(total.saturating_sub(free))),
                ("volume_name", Value::str(p.fs_uuid.clone())),
            ]));
        }
    }
    out
}

/// Subdirectories (or entries, without `dir_only`) of `path`.
fn folder(path: &str, dir_only: bool) -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(path) else { return Vec::new() };
    let mut entries: Vec<(String, bool)> = rd
        .filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir())))
        .filter(|(_, d)| *d || !dir_only)
        .collect();
    entries.sort();
    entries
        .into_iter()
        .map(|(n, is_dir)| {
            let full = format!("{}/{n}", path.trim_end_matches('/'));
            Value::map([("is_dir", Value::Int(is_dir as u64)), ("name", Value::str(n)), ("path", Value::str(full))])
        })
        .collect()
}

pub fn answer(sess: &Session, ev: &Event) {
    let Some(task) = ev.body.get("webapi_task") else { return };
    let sub = task.get("subaction").and_then(Value::as_str).unwrap_or("");
    let event_id = task.get("event_id").and_then(Value::as_int).unwrap_or(ev.id);
    let list = match sub {
        "list_nodes" => {
            let path = task.get("node_path").and_then(Value::as_str).unwrap_or("");
            let dir_only = task.get("dir_only").and_then(Value::as_int) == Some(1);
            if path.is_empty() { volumes() } else { folder(path, dir_only) }
        }
        _ => {
            eprintln!("act_client {sub}: not supported");
            return;
        }
    };
    let r = sess.connect().and_then(|mut c| {
        sess.call(
            &mut c,
            "act_client_result",
            vec![("error_code", Value::Int(0)), ("event_id", Value::Int(event_id)), ("node_list", Value::Array(list))],
        )
    });
    if let Err(e) = r {
        eprintln!("act_client_result: {e}");
    }
}
