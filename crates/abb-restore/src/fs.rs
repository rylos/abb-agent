//! Work on restored filesystems: growing them to their new partition, and the EFI fallback
//! boot path.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A temporary mount, removed on drop.
struct Mount(PathBuf);

impl Mount {
    fn new(dev: &Path, fstype: &str) -> Result<Mount, String> {
        Mount::with(dev, fstype, "")
    }

    fn with(dev: &Path, fstype: &str, options: &str) -> Result<Mount, String> {
        let dir = PathBuf::from(format!("/run/abb-restore/mnt-{}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let src = CString::new(dev.as_os_str().as_bytes()).unwrap();
        let dst = CString::new(dir.as_os_str().as_bytes()).unwrap();
        let ty = CString::new(fstype).unwrap();
        let opts = CString::new(options).unwrap();
        let data = if options.is_empty() { std::ptr::null() } else { opts.as_ptr() as *const libc::c_void };
        let r = unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), ty.as_ptr(), 0, data) };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            let _ = fs::remove_dir(&dir);
            return Err(format!("cannot mount {} ({fstype}): {e}", dev.display()));
        }
        Ok(Mount(dir))
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let dst = CString::new(self.0.as_os_str().as_bytes()).unwrap();
        unsafe {
            libc::sync();
            libc::umount(dst.as_ptr());
        }
        let _ = fs::remove_dir(&self.0);
    }
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    // e2fsck: 0 no errors, 1 errors corrected.
    if out.status.success() || (cmd == "e2fsck" && out.status.code() == Some(1)) {
        Ok(())
    } else {
        Err(format!("{cmd} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Grows the filesystem on `dev` to the size of the partition.
pub fn grow(dev: &Path, fs_type: &str) -> Result<(), String> {
    let d = dev.to_string_lossy();
    match fs_type {
        t if t.starts_with("ext") => {
            run("e2fsck", &["-f", "-y", &d])?;
            run("resize2fs", &[&d])
        }
        "btrfs" => {
            // BTRFS_IOC_RESIZE with "max" on the only device (devid 1).
            let m = Mount::new(dev, "btrfs")?;
            let dir = fs::File::open(&m.0).map_err(|e| e.to_string())?;
            let mut args = [0u8; 4096];
            args[8..11].copy_from_slice(b"max");
            const BTRFS_IOC_RESIZE: u32 = 0x5000_9403;
            if unsafe { libc::ioctl(dir.as_raw_fd(), BTRFS_IOC_RESIZE as _, args.as_mut_ptr()) } != 0 {
                return Err(format!("resize: {}", std::io::Error::last_os_error()));
            }
            Ok(())
        }
        other => Err(format!("growing {other} is not supported")),
    }
}

/// Is the root item of subvolume `id` still in the root tree? (BTRFS_IOC_TREE_SEARCH)
fn root_exists(dir: &fs::File, id: u64) -> Result<bool, String> {
    const TREE_SEARCH: u32 = 0xd000_9411;
    let mut args = [0u8; 4096];
    let key = [1u64, id, id, 0, u64::MAX, 0, u64::MAX];
    for (i, v) in key.iter().enumerate() {
        args[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    args[56..60].copy_from_slice(&132u32.to_le_bytes()); // min_type: ROOT_ITEM
    args[60..64].copy_from_slice(&132u32.to_le_bytes()); // max_type
    args[64..68].copy_from_slice(&1u32.to_le_bytes()); // nr_items
    if unsafe { libc::ioctl(dir.as_raw_fd(), TREE_SEARCH as _, args.as_mut_ptr()) } != 0 {
        return Err(format!("tree search: {}", std::io::Error::last_os_error()));
    }
    Ok(u32::from_le_bytes(args[64..68].try_into().unwrap()) > 0)
}

/// On the btrfs at `dev`: waits until the subvolumes deleted at restore time (`deleted`) are
/// gone, as `btrfs subvolume sync` does (until then their extents still count as used, and a
/// scrub would report their missing data), then creates `recreate` empty so that mounts in
/// /etc/fstab still find them.
pub fn finish_exclusion(dev: &Path, recreate: &[PathBuf], deleted: &[u64]) -> Result<(), String> {
    let m = Mount::with(dev, "btrfs", "subvolid=5")?;
    let dir = fs::File::open(&m.0).map_err(|e| e.to_string())?;
    const SYNC: u32 = 0x9408;
    let start = std::time::Instant::now();
    for &id in deleted {
        while root_exists(&dir, id)? {
            if start.elapsed() > std::time::Duration::from_secs(1800) {
                return Err("btrfs is still dropping the subvolumes left out (it goes on at the next mount)".into());
            }
            unsafe { libc::ioctl(dir.as_raw_fd(), SYNC as _) };
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    let mut paths = recreate.to_vec();
    paths.sort();
    for p in &paths {
        let target = m.0.join(p);
        if target.exists() {
            continue;
        }
        let parent = target.parent().unwrap_or(&m.0);
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        btrfs_image::snapshot::create_subvolume(parent, &name).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Firmware looks for this file when no NVRAM boot entry points to the disk (a new machine).
#[cfg(target_arch = "aarch64")]
const FALLBACK: &str = "BOOTAA64.EFI";
#[cfg(not(target_arch = "aarch64"))]
const FALLBACK: &str = "BOOTX64.EFI";

/// Case-insensitive lookup of `name` in `dir` (FAT names).
fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))
        .map(|e| e.path())
}

/// Makes sure the EFI system partition boots without an NVRAM entry: if `\EFI\BOOT\BOOTX64.EFI`
/// is missing, copies systemd-boot, shim (with GRUB) or GRUB there. Returns what was done.
pub fn efi_fallback(esp: &Path) -> Result<Option<String>, String> {
    let m = Mount::new(esp, "vfat")?;
    let efi = find(&m.0, "EFI").ok_or("no EFI directory")?;
    if find(&efi, "BOOT").and_then(|b| find(&b, FALLBACK)).is_some() {
        return Ok(None);
    }
    let mut candidates: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    let arch = FALLBACK.trim_start_matches("BOOT").trim_end_matches(".EFI").to_lowercase();
    if let Some(sd) = find(&efi, "systemd").and_then(|d| find(&d, &format!("systemd-boot{arch}.efi"))) {
        candidates.push((sd, vec![]));
    }
    for dir in fs::read_dir(&efi).map_err(|e| e.to_string())?.filter_map(Result::ok).map(|e| e.path()) {
        if let Some(shim) = find(&dir, &format!("shim{arch}.efi")) {
            let grub = find(&dir, &format!("grub{arch}.efi")).into_iter().collect();
            candidates.push((shim, grub));
        } else if let Some(grub) = find(&dir, &format!("grub{arch}.efi")) {
            candidates.push((grub, vec![]));
        }
    }
    let Some((loader, extra)) = candidates.into_iter().next() else {
        return Err("no boot loader found to use as the EFI fallback".into());
    };
    let boot = match find(&efi, "BOOT") {
        Some(b) => b,
        None => {
            let b = efi.join("BOOT");
            fs::create_dir(&b).map_err(|e| e.to_string())?;
            b
        }
    };
    fs::copy(&loader, boot.join(FALLBACK)).map_err(|e| e.to_string())?;
    for f in extra {
        fs::copy(&f, boot.join(f.file_name().unwrap())).map_err(|e| e.to_string())?;
    }
    let rel = loader.strip_prefix(&m.0).unwrap_or(&loader).display().to_string();
    Ok(Some(format!("copied {rel} to EFI/BOOT/{FALLBACK} (boot without an NVRAM entry)")))
}
