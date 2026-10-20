//! Subvolumes left out of the image do not exist in a restored system, but /etc/fstab may
//! still mount them (a package cache, a games library): the boot would stop. Before every
//! backup the agent records which subvolumes it leaves out, in a file that is part of the
//! image; early at boot, before the local filesystems are mounted, `abb-agent restore-fixup`
//! recreates the missing ones, empty. On a system that was not restored it does nothing.

use crate::state::STATE_DIR;
use btrfs_image::prepare::{top_mount, top_unmount};
use btrfs_image::snapshot::create_subvolume;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn path() -> PathBuf {
    Path::new(STATE_DIR).join("excluded.json")
}

type Record = BTreeMap<String, Vec<PathBuf>>;

fn load() -> Record {
    std::fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Records the subvolumes left out of filesystem `uuid` (written before the snapshots).
pub fn record(uuid: &str, paths: &[PathBuf]) -> std::io::Result<()> {
    let mut r = load();
    if paths.is_empty() {
        if r.remove(uuid).is_none() {
            return Ok(());
        }
    } else {
        r.insert(uuid.to_string(), paths.to_vec());
    }
    std::fs::create_dir_all(STATE_DIR)?;
    let tmp = path().with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&r).map_err(std::io::Error::other)?)?;
    let f = std::fs::File::open(&tmp)?;
    f.sync_all()?;
    std::fs::rename(tmp, path())
}

/// A device of the mounted btrfs filesystem `uuid`, from /sys/fs/btrfs.
fn device_of(uuid: &str) -> Option<PathBuf> {
    let dir = Path::new("/sys/fs/btrfs").join(uuid).join("devices");
    let name = std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).next()?.file_name();
    Some(Path::new("/dev").join(name))
}

/// Recreates, empty, the recorded subvolumes that are missing.
pub fn run() {
    for (uuid, paths) in load() {
        let Some(dev) = device_of(&uuid) else { continue };
        let (top, ours) = match top_mount(&dev, &uuid) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{uuid}: {e}");
                continue;
            }
        };
        let mut paths = paths;
        paths.sort(); // parents before children
        for p in paths {
            let full = top.join(&p);
            let (Some(parent), Some(name)) = (full.parent(), full.file_name()) else { continue };
            if full.exists() || !parent.is_dir() {
                continue;
            }
            match create_subvolume(parent, &name.to_string_lossy()) {
                Ok(()) => eprintln!("recreated the excluded subvolume {} (empty)", p.display()),
                Err(e) => eprintln!("recreating {}: {e}", p.display()),
            }
        }
        if ours {
            top_unmount(&top);
        }
    }
}
