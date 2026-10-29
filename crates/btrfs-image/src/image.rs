//! Copies the used blocks to an image file (for tests and diagnostics).
//!
//! The real agent sends the same bytes to the ABB server instead of writing them to a file;
//! the superblock and node verification logic is the same.

use crate::disk::*;
use crate::capture::Capture;
use crate::usedmap::{Kind, UsedMap};
use crate::{Error, Filesystem};
use std::fs::File;
use std::os::unix::fs::FileExt;

/// Bytes to send for a physical range: captured blocks come from the capture, the others from
/// the device (nodes are verified); superblocks are replaced with the version from the
/// generation read, without the tree-log.
pub fn range_bytes(
    fs: &Filesystem,
    cap: &Capture,
    physical: u64,
    len: u64,
    kind: Kind,
    logical: u64,
) -> Result<Vec<u8>, Error> {
    let captured = match kind {
        Kind::Tree => cap.nodes.get(&logical),
        Kind::Data => cap.data.get(&logical),
        Kind::Reserved => None,
    };
    if let Some(bytes) = captured {
        return Ok(bytes.clone());
    }
    match kind {
        // The node verified from either copy, for both copies: a bad copy on the device is
        // repaired in the image.
        Kind::Tree => Ok(fs.read_node(logical, 0)?.data),
        Kind::Data => {
            let mut buf = vec![0u8; len as usize];
            fs.read_at(&mut buf, physical)?;
            Ok(buf)
        }
        Kind::Reserved => {
            let mut buf = vec![0u8; len as usize];
            fs.read_at(&mut buf, physical)?;
            for &m in &SUPER_MIRRORS {
                if m >= physical && m + SUPER_SIZE as u64 <= physical + len {
                    let o = (m - physical) as usize;
                    buf[o..o + SUPER_SIZE].copy_from_slice(&fs.sb.without_log(m));
                }
            }
            Ok(buf)
        }
    }
}

/// Writes all used ranges to `out` (a sparse file the size of the device).
pub fn write_image(fs: &Filesystem, cap: &Capture, map: &UsedMap, out: &File) -> Result<u64, Error> {
    out.set_len(fs.sb.total_bytes)?;
    let mut written = 0;
    for r in &map.ranges {
        let buf = range_bytes(fs, cap, r.physical, r.len, r.kind, r.logical)?;
        out.write_all_at(&buf, r.physical)?;
        written += r.len;
    }
    Ok(written)
}

/// Prepares the btrfs volume bytes in `[start, start + buf.len())` (physical offsets in the
/// volume). `buf` must hold the bytes just read from the device for that window: unused areas
/// are zeroed, captured blocks overwritten, nodes verified in place and superblocks replaced.
/// Returns `false` if the window contains no used bytes.
pub fn fill_window(fs: &Filesystem, cap: &Capture, map: &UsedMap, start: u64, buf: &mut [u8]) -> Result<bool, Error> {
    let end = start + buf.len() as u64;
    // First range that may overlap the window: ranges do not overlap, so their ends are sorted
    // too and the search is exact (the margin only makes it look further back).
    let first = map.ranges.partition_point(|r| r.physical + r.len <= start.saturating_sub(128 << 20));
    let mut cursor = start; // the window is already done up to here
    let mut any = false;
    for r in &map.ranges[first..] {
        if r.physical >= end {
            break;
        }
        let (rs, re) = (r.physical.max(start), (r.physical + r.len).min(end));
        if rs >= re {
            continue;
        }
        any = true;
        if rs > cursor {
            buf[(cursor - start) as usize..(rs - start) as usize].fill(0);
        }
        let dst = &mut buf[(rs - start) as usize..(re - start) as usize];
        let src_off = (rs - r.physical) as usize;
        match r.kind {
            Kind::Tree => {
                if let Some(node) = cap.nodes.get(&r.logical) {
                    dst.copy_from_slice(&node[src_off..src_off + dst.len()]);
                } else if rs == r.physical && re == r.physical + r.len && fs.verify_node(r.logical, r.generation, dst).is_ok() {
                    // Verified in place. The allocation transaction of the extent is the node's
                    // generation: an older node still there (a lost write) does not pass.
                } else {
                    // Node straddling the window, or this copy failed: verified in full, from
                    // whichever copy is good (DUP).
                    let node = fs.read_node(r.logical, r.generation)?;
                    dst.copy_from_slice(&node.data[src_off..src_off + dst.len()]);
                }
            }
            Kind::Data => {
                if let Some(data) = cap.data.get(&r.logical) {
                    let src = data.get(src_off..src_off + dst.len()).ok_or_else(|| {
                        Error::Format(format!("captured extent at {} is {} bytes, not {}", r.logical, data.len(), r.len))
                    })?;
                    dst.copy_from_slice(src);
                }
            }
            Kind::Reserved => {
                for &m in &SUPER_MIRRORS {
                    let (ms, me) = (m, m + SUPER_SIZE as u64);
                    if ms < re && me > rs {
                        let sb = fs.sb.without_log(m);
                        let (os, oe) = (ms.max(rs), me.min(re));
                        buf[(os - start) as usize..(oe - start) as usize]
                            .copy_from_slice(&sb[(os - ms) as usize..(oe - ms) as usize]);
                    }
                }
            }
        }
        cursor = cursor.max(re);
    }
    if any && cursor < end {
        buf[(cursor - start) as usize..].fill(0);
    }
    Ok(any)
}
