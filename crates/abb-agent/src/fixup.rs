//! Subvolumes left out of the image do not exist in a restored system, but /etc/fstab may
//! still mount them (a package cache, a games library): the boot would stop. Before every
//! backup the agent records which subvolumes it leaves out, in a file that is part of the
//! image; early at boot, before the local filesystems are mounted, `abb-agent restore-fixup`
//! recreates the missing ones, empty. On a system that was not restored it does nothing.
//!
//! In the image an excluded subvolume looks exactly like one deleted with `btrfs subvolume
//! delete`, so a subvolume the user deleted after the last backup looks the same: only those a
//! mount still needs are recreated (a `subvol=` option in /etc/fstab or in a systemd mount unit
//! naming them or a subvolume inside them). If /etc/fstab cannot be read, all are.

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
    crate::state::write_atomic(&path(), serde_json::to_string_pretty(&r).map_err(std::io::Error::other)?.as_bytes(), 0o644)
}

/// A device of the mounted btrfs filesystem `uuid`, from /sys/fs/btrfs.
fn device_of(uuid: &str) -> Option<PathBuf> {
    let dir = Path::new("/sys/fs/btrfs").join(uuid).join("devices");
    let name = std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).next()?.file_name();
    Some(Path::new("/dev").join(name))
}

/// Subvolume paths named by the `subvol=` mount options in `texts` (fstab, mount units), from
/// the top level.
fn mounted_subvolumes(texts: &[String]) -> Vec<PathBuf> {
    texts
        .iter()
        .flat_map(|t| t.lines())
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.split(|c: char| c == ',' || c.is_whitespace()))
        .filter_map(|o| o.strip_prefix("subvol=").or_else(|| o.strip_prefix("Options=subvol=")))
        .map(|v| PathBuf::from(v.trim_start_matches('/')))
        .collect()
}

/// The mount options of /etc/fstab and the systemd mount units in /etc; `None` without fstab.
fn mount_options() -> Option<Vec<String>> {
    let mut texts = vec![std::fs::read_to_string("/etc/fstab").ok()?];
    if let Ok(rd) = std::fs::read_dir("/etc/systemd/system") {
        for e in rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "mount")) {
            texts.extend(std::fs::read_to_string(e.path()).ok());
        }
    }
    Some(texts)
}

/// Recreates, empty, the recorded subvolumes that are missing and that a mount needs.
pub fn run() {
    let mounted = mount_options().map(|t| mounted_subvolumes(&t));
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
            if mounted.as_ref().is_some_and(|m| !m.iter().any(|r| r.starts_with(&p))) {
                eprintln!("excluded subvolume {} missing but not mounted: not recreated", p.display());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subvolumes_named_by_mounts() {
        let fstab = "\
UUID=1 /  btrfs rw,noatime,subvol=/@ 0 0
UUID=1 /var/cache/pacman/pkg btrfs rw,subvol=@pkg,compress=zstd 0 0
# UUID=1 /old btrfs subvol=/@old 0 0
UUID=1 /games btrfs subvol=/@games/steam 0 0
";
        let unit = "[Mount]\nWhat=/dev/sda2\nWhere=/data\nOptions=subvol=@data,noatime\n";
        let m = mounted_subvolumes(&[fstab.into(), unit.into()]);
        assert_eq!(m, ["@", "@pkg", "@games/steam", "@data"].map(PathBuf::from));
        let needed = |p: &str| m.iter().any(|r| r.starts_with(p));
        assert!(needed("@pkg") && needed("@games") && needed("@data"));
        assert!(!needed("@old") && !needed("@snapshots") && !needed("@pk"));
    }
}
