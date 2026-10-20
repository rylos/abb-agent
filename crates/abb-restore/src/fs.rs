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
        let dir = PathBuf::from(format!("/run/abb-restore/mnt-{}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let src = CString::new(dev.as_os_str().as_bytes()).unwrap();
        let dst = CString::new(dir.as_os_str().as_bytes()).unwrap();
        let ty = CString::new(fstype).unwrap();
        let r = unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), ty.as_ptr(), 0, std::ptr::null()) };
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
