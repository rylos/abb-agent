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
    /// aligned window containing them is read and the requested part is copied.
    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        let offset = offset + self.base;
        let start = offset & !(ALIGN as u64 - 1);
        let end = (offset + buf.len() as u64).next_multiple_of(ALIGN as u64);
        let mut tmp = AlignedBuf::new((end - start) as usize);
        self.file.read_exact_at(tmp.as_mut_slice(), start)?;
        let skip = (offset - start) as usize;
        buf.copy_from_slice(&tmp.as_slice()[skip..skip + buf.len()]);
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
