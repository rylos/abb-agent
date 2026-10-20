//! Read-only snapshots of all subvolumes, via the btrfs ioctls.
//!
//! Works on a directory where the volume root (subvolid=5) is mounted. The snapshots keep the
//! subvolumes' data and nodes pinned at the backup generation, so their blocks cannot be
//! reused while the image is being read.

use crate::disk::*;
use crate::{Error, Filesystem};
use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

const ROOT_REF_KEY: u8 = 156;
const FIRST_FREE_OBJECTID: u64 = 256;
/// The ids of "special" trees (data reloc, log, …) are negative numbers as u64.
const LAST_FREE_OBJECTID: u64 = u64::MAX - 256;
const SUBVOL_RDONLY: u64 = 1 << 1;

// _IOW(0x94, 23, struct btrfs_ioctl_vol_args_v2), sizeof = 4096
const IOC_SNAP_CREATE_V2: libc::c_ulong = 0x5000_9417;
// _IOW(0x94, 15, struct btrfs_ioctl_vol_args), sizeof = 4096
const IOC_SNAP_DESTROY: libc::c_ulong = 0x5000_940f;
// _IOWR(0x94, 18, struct btrfs_ioctl_ino_lookup_args), sizeof = 4096
const IOC_INO_LOOKUP: libc::c_ulong = 0xd000_9412;
// _IOW(0x94, 14, struct btrfs_ioctl_vol_args), sizeof = 4096
const IOC_SUBVOL_CREATE: libc::c_ulong = 0x5000_940e;
/// _IO(0x94, 8)
const IOC_SYNC: libc::c_ulong = 0x9408;

#[derive(Clone, Debug)]
pub struct Subvolume {
    pub id: u64,
    pub parent: u64,
    /// Path relative to the volume root, e.g. `@home` or `@/var/lib/machines`.
    pub path: PathBuf,
}

#[repr(C)]
struct VolArgsV2 {
    fd: i64,
    transid: u64,
    flags: u64,
    unused: [u64; 4],
    name: [u8; 4040],
}

#[repr(C)]
struct VolArgs {
    fd: i64,
    name: [u8; 4088],
}

#[repr(C)]
struct InoLookupArgs {
    treeid: u64,
    objectid: u64,
    name: [u8; 4080],
}

/// Error from the last system call, with context; the `kind` is kept from the errno, the
/// numeric code is not.
fn os_err(what: &str) -> Error {
    Error::Io(std::io::Error::new(
        std::io::Error::last_os_error().kind(),
        format!("{what}: {}", std::io::Error::last_os_error()),
    ))
}

fn open_dir(path: &Path) -> Result<File, Error> {
    Ok(File::open(path)?)
}

/// Subvolumes of the volume (excluding top level 5), from the device's root tree.
pub fn list_subvolumes(fs: &Filesystem, top: &Path) -> Result<Vec<Subvolume>, Error> {
    let mut refs = Vec::new();
    fs.walk(fs.sb.root, 0, &mut |_, key, data| {
        if key.kind == ROOT_REF_KEY
            && (FIRST_FREE_OBJECTID..=LAST_FREE_OBJECTID).contains(&key.offset)
        {
            let dirid = u64_at(data, 0);
            let name_len = u16_at(data, 16) as usize;
            let name = String::from_utf8_lossy(&data[18..18 + name_len]).into_owned();
            refs.push((key.objectid, key.offset, dirid, name));
        }
        Ok(())
    })?;

    // A subvolume's path depends on its parent's: resolve them in waves.
    let top_fd = open_dir(top)?;
    let mut done: Vec<Subvolume> = Vec::new();
    // Deleted after the commit read (and their children): they no longer exist.
    let mut gone: Vec<u64> = Vec::new();
    let mut pending = refs;
    while !pending.is_empty() {
        let before = pending.len();
        let mut rest = Vec::new();
        for (parent, id, dirid, name) in pending {
            if gone.contains(&parent) {
                gone.push(id);
                continue;
            }
            let parent_path = if parent == 5 {
                Some(PathBuf::new())
            } else {
                done.iter().find(|s| s.id == parent).map(|s| s.path.clone())
            };
            match parent_path {
                Some(pp) => match ino_lookup(&top_fd, parent, dirid) {
                    Ok(dir) => done.push(Subvolume { id, parent, path: pp.join(dir).join(name) }),
                    Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => gone.push(id),
                    Err(e) => return Err(e),
                },
                None => rest.push((parent, id, dirid, name)),
            }
        }
        if rest.len() == before {
            return Err(Error::Format("subvolumes with parent not found".into()));
        }
        pending = rest;
    }
    done.sort_by_key(|s| s.id);
    Ok(done)
}

/// Path (relative to subvolume `treeid`) of directory `objectid`.
fn ino_lookup(top: &File, treeid: u64, objectid: u64) -> Result<PathBuf, Error> {
    let mut args = InoLookupArgs { treeid, objectid, name: [0; 4080] };
    if unsafe { libc::ioctl(top.as_raw_fd(), IOC_INO_LOOKUP as _, &mut args) } < 0 {
        return Err(os_err("BTRFS_IOC_INO_LOOKUP"));
    }
    let len = args.name.iter().position(|&b| b == 0).unwrap_or(0);
    Ok(PathBuf::from(String::from_utf8_lossy(&args.name[..len]).into_owned()))
}

/// Commits the running transaction (BTRFS_IOC_SYNC): the on-disk superblock becomes current.
pub fn sync(top: &Path) -> Result<(), Error> {
    let f = open_dir(top)?;
    if unsafe { libc::ioctl(f.as_raw_fd(), IOC_SYNC as _) } < 0 {
        return Err(os_err("BTRFS_IOC_SYNC"));
    }
    Ok(())
}

/// Creates the empty subvolume `name` in `parent_dir`.
pub fn create_subvolume(parent_dir: &Path, name: &str) -> Result<(), Error> {
    let dir = open_dir(parent_dir)?;
    let mut args = VolArgs { fd: 0, name: [0; 4088] };
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() >= args.name.len() || bytes.contains(&b'/') {
        return Err(Error::Format(format!("invalid subvolume name {name:?}")));
    }
    args.name[..bytes.len()].copy_from_slice(bytes);
    if unsafe { libc::ioctl(dir.as_raw_fd(), IOC_SUBVOL_CREATE as _, &args) } < 0 {
        return Err(os_err(&format!("creating subvolume {name}")));
    }
    Ok(())
}

/// Creates in `dest_dir` the read-only snapshot `name` of the subvolume mounted at `source`.
pub fn create_readonly(source: &Path, dest_dir: &Path, name: &str) -> Result<(), Error> {
    let src = open_dir(source)?;
    let dst = open_dir(dest_dir)?;
    let mut args = VolArgsV2 {
        fd: src.as_raw_fd() as i64,
        transid: 0,
        flags: SUBVOL_RDONLY,
        unused: [0; 4],
        name: [0; 4040],
    };
    let cname = CString::new(name).map_err(|_| Error::Format("snapshot name contains NUL".into()))?;
    let bytes = cname.as_bytes();
    if bytes.len() >= args.name.len() {
        return Err(Error::Format("snapshot name too long".into()));
    }
    args.name[..bytes.len()].copy_from_slice(bytes);
    if unsafe { libc::ioctl(dst.as_raw_fd(), IOC_SNAP_CREATE_V2 as _, &args) } < 0 {
        return Err(os_err(&format!("snapshot of {}", source.display())));
    }
    Ok(())
}

/// Deletes subvolume `name` inside `parent_dir`.
pub fn destroy(parent_dir: &Path, name: &str) -> Result<(), Error> {
    let dir = open_dir(parent_dir)?;
    let mut args = VolArgs { fd: 0, name: [0; 4088] };
    let bytes = name.as_bytes();
    if bytes.len() >= args.name.len() {
        return Err(Error::Format("subvolume name too long".into()));
    }
    args.name[..bytes.len()].copy_from_slice(bytes);
    if unsafe { libc::ioctl(dir.as_raw_fd(), IOC_SNAP_DESTROY as _, &args) } < 0 {
        return Err(os_err(&format!("deleting {name}")));
    }
    Ok(())
}
