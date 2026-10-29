//! Block device reads with O_DIRECT.
//!
//! btrfs writes metadata and superblocks without going through the block device's page cache:
//! a buffered read of `/dev/…` could return stale contents already in the cache.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

/// Alignment required by O_DIRECT (enough for 512- and 4096-byte sectors).
pub const ALIGN: usize = 4096;

pub struct Device {
    file: File,
    /// Offset of the filesystem in the file (a partition inside a whole disk).
    base: u64,
}

impl Device {
    pub fn open(path: &Path) -> std::io::Result<Device> {
        Device::open_at(path, 0)
    }

    /// A filesystem starting `base` bytes into `path` (e.g. a partition of a disk whose table
    /// the kernel has not read yet).
    pub fn open_at(path: &Path, base: u64) -> std::io::Result<Device> {
        let file = OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(path)?;
        Ok(Device { file, base })
    }

    /// Reads `buf.len()` bytes from `offset`. Offset and length need not be aligned: the
    /// aligned window containing them is read and the requested part is copied. The window may
    /// pass the end of a device whose size is not a multiple of `ALIGN`: the read stops short
    /// there, which is fine as long as the requested bytes came.
    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        let offset = offset + self.base;
        let start = offset & !(ALIGN as u64 - 1);
        let end = (offset + buf.len() as u64).next_multiple_of(ALIGN as u64);
        let mut tmp = AlignedBuf::new((end - start) as usize);
        let skip = (offset - start) as usize;
        let need = skip + buf.len();
        let window = &mut tmp.as_mut_slice()[..(end - start) as usize];
        let mut got = 0;
        while got < need {
            match self.file.read_at(&mut window[got..], start + got as u64) {
                Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        buf.copy_from_slice(&tmp.as_slice()[skip..need]);
        Ok(())
    }
}

struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuf {
    fn new(len: usize) -> AlignedBuf {
        let layout = Layout::from_size_align(len.max(ALIGN), ALIGN).unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "allocation failed");
        AlignedBuf { ptr, layout }
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_tail_of_a_size_not_multiple_of_align() {
        let path = std::env::temp_dir().join(format!("abb-device-{}", std::process::id()));
        let data: Vec<u8> = (0..10240u32 + 512).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let dev = match Device::open(&path) {
            Ok(d) => d,
            // No O_DIRECT on this filesystem.
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => return std::fs::remove_file(&path).unwrap(),
            Err(e) => panic!("{e}"),
        };
        let mut buf = vec![0u8; 700];
        dev.read_exact_at(&mut buf, data.len() as u64 - 700).unwrap();
        assert_eq!(buf, data[data.len() - 700..]);
        let mut buf = vec![0u8; 100];
        dev.read_exact_at(&mut buf, 5000).unwrap();
        assert_eq!(buf, data[5000..5100]);
        // Past the end: an error.
        assert!(dev.read_exact_at(&mut [0u8; 10], data.len() as u64 - 5).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
