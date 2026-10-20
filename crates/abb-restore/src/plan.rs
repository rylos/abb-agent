//! What goes where: the mapping from the disks and volumes of a backup version to the local
//! disks, checked before anything is written.

use crate::gpt::Gpt;
use crate::local::{LocalDisk, size_str};
use crate::spec::{Disk, Partition, Version};

const MIB: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every disk of the backup onto a whole local disk, partition table included.
    Device,
    /// Only the volumes needed to boot, onto the disk they came from (same layout).
    System,
    /// Chosen volumes onto existing partitions or free space.
    Volumes,
}

impl Mode {
    /// `restore_mode` of the protocol.
    pub fn code(self) -> u64 {
        match self {
            Mode::Device => 1,
            Mode::System => 2,
            Mode::Volumes => 3,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Mode::Device => "Entire device",
            Mode::System => "System volumes",
            Mode::Volumes => "Selected volumes",
        }
    }
}

/// Where a volume goes in [`Mode::Volumes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// Over an existing partition (by number).
    Partition(u64),
    /// A new partition at the start of the free area beginning at this sector.
    Free(u64),
}

#[derive(Clone, Debug)]
pub struct VolumeTarget {
    pub disk: usize,
    pub part: usize,
    /// Index in the list of local disks.
    pub local: usize,
    pub place: Place,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub mode: Mode,
    /// Device and system modes: local disk index for each disk of the version (None: skipped).
    pub disks: Vec<Option<usize>>,
    /// Volumes mode.
    pub volumes: Vec<VolumeTarget>,
    /// Grow the last partition (device mode) or the restored volumes (volumes mode) and their
    /// filesystem to use the extra space.
    pub grow: bool,
    /// Make sure the EFI system partition can be booted without an NVRAM entry.
    pub efi_fallback: bool,
}

/// End of the last partition of a disk of the backup (bytes).
pub fn used_end(d: &Disk) -> u64 {
    d.partitions.iter().map(|p| p.offset + p.length).max().unwrap_or(1 << 20)
}

/// Room needed at the end of a GPT disk for the backup table.
fn gpt_tail(d: &Disk) -> u64 {
    33 * d.sector_size
}

/// Can disk `d` of the backup go on local disk `l`?
pub fn disk_fits(d: &Disk, l: &LocalDisk) -> Result<(), String> {
    if let Some(why) = &l.busy {
        return Err(why.clone());
    }
    if d.gpt && l.sector_size != d.sector_size {
        return Err(format!("sector size {} instead of {}", l.sector_size, d.sector_size));
    }
    let need = if d.gpt { used_end(d) + gpt_tail(d) } else { d.size };
    if l.size < need {
        return Err(format!("too small: {} needed", size_str(need)));
    }
    Ok(())
}

/// Picks local disks for the disks of the backup: the smallest that fits, biggest disks first.
pub fn auto_map(v: &Version, locals: &[LocalDisk]) -> Vec<Option<usize>> {
    let mut order: Vec<usize> = (0..v.disks.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(v.disks[i].size));
    let mut taken = vec![false; locals.len()];
    let mut out = vec![None; v.disks.len()];
    for i in order {
        let best = (0..locals.len())
            .filter(|&j| !taken[j] && disk_fits(&v.disks[i], &locals[j]).is_ok())
            .min_by_key(|&j| locals[j].size);
        if let Some(j) = best {
            taken[j] = true;
            out[i] = Some(j);
        }
    }
    out
}

/// Free areas (first, last sector) of local disk `l`, minus the new partitions of `taken`.
pub fn free_areas(v: &Version, l: &LocalDisk, local: usize, taken: &[VolumeTarget]) -> Vec<(u64, u64)> {
    let align = MIB / l.sector_size;
    let mut areas = l.gpt.clone().unwrap_or_else(|| Gpt::new(l.sector_size)).free(l.size);
    for t in taken.iter().filter(|t| t.local == local) {
        let Place::Free(start) = t.place else { continue };
        let end = start + v.disks[t.disk].partitions[t.part].length.div_ceil(l.sector_size) - 1;
        let mut next = Vec::new();
        for (a, b) in areas {
            if end < a || start > b {
                next.push((a, b));
                continue;
            }
            if start > a {
                next.push((a, start - 1));
            }
            let after = (end + 1).div_ceil(align) * align;
            if after <= b {
                next.push((after, b));
            }
        }
        areas = next;
    }
    areas
}

/// First free area of local disk `local` where volume `part` of disk `disk` fits, after the
/// volumes already placed.
pub fn first_fit(v: &Version, locals: &[LocalDisk], local: usize, disk: usize, part: usize, taken: &[VolumeTarget]) -> Option<u64> {
    let l = &locals[local];
    let need = v.disks[disk].partitions[part].length;
    free_areas(v, l, local, taken).into_iter().find(|(a, b)| (b - a + 1) * l.sector_size >= need).map(|(a, _)| a)
}

/// Space a new or existing partition offers for `p` (bytes), or why it cannot take it. `free`
/// are the free areas left by the volumes placed before.
pub fn place_room(p: &Partition, l: &LocalDisk, place: &Place, free: &[(u64, u64)]) -> Result<u64, String> {
    match place {
        Place::Partition(n) => {
            if let Some(why) = l.partitions.iter().find(|q| q.number == *n).and_then(|q| q.busy.as_ref()) {
                return Err(why.clone());
            }
        }
        // The kernel cannot reread the partition table of a disk in use.
        Place::Free(_) => {
            if let Some(why) = &l.busy {
                return Err(why.clone());
            }
        }
    }
    let room = match place {
        Place::Partition(n) => {
            l.partitions.iter().find(|q| q.number == *n).ok_or_else(|| format!("no partition {n}"))?.length
        }
        Place::Free(start) => {
            let (_, b) = free
                .iter()
                .find(|(a, b)| a <= start && start <= b)
                .ok_or_else(|| "not in a free area".to_string())?;
            (b - start + 1) * l.sector_size
        }
    };
    if room < p.length {
        return Err(format!("too small: {} needed, {} available", size_str(p.length), size_str(room)));
    }
    Ok(room)
}

impl Plan {
    /// Checks the whole plan. Returns the problems found (empty if it can run).
    pub fn check(&self, v: &Version, locals: &[LocalDisk]) -> Vec<String> {
        let mut errs = Vec::new();
        match self.mode {
            Mode::Device | Mode::System => {
                let mut used = Vec::new();
                for (i, m) in self.disks.iter().enumerate() {
                    let Some(j) = *m else { continue };
                    if used.contains(&j) {
                        errs.push(format!("{} chosen twice", locals[j].name));
                    }
                    used.push(j);
                    if let Err(e) = disk_fits(&v.disks[i], &locals[j]) {
                        errs.push(format!("disk {} → {}: {e}", v.disks[i].name(), locals[j].name));
                    }
                    if self.mode == Mode::System
                        && let Err(e) = same_layout(&v.disks[i], &locals[j])
                    {
                        errs.push(format!("disk {} → {}: {e}", v.disks[i].name(), locals[j].name));
                    }
                }
                if used.is_empty() {
                    errs.push("no disk chosen".into());
                }
            }
            Mode::Volumes => {
                if self.volumes.is_empty() {
                    errs.push("no volume chosen".into());
                }
                for (k, t) in self.volumes.iter().enumerate() {
                    let p = &v.disks[t.disk].partitions[t.part];
                    let free = free_areas(v, &locals[t.local], t.local, &self.volumes[..k]);
                    if let Err(e) = place_room(p, &locals[t.local], &t.place, &free) {
                        errs.push(format!("{} → {}: {e}", p.label, locals[t.local].name));
                    }
                    if self.volumes[..k].iter().any(|o| o.local == t.local && o.place == t.place) {
                        errs.push(format!("{}: target chosen twice", p.label));
                    }
                }
            }
        }
        errs
    }
}

/// System mode writes the boot volumes back in place: the local disk must have the partitions of
/// the backup at the same positions (at least as big), or no partition table at all.
pub fn same_layout(d: &Disk, l: &LocalDisk) -> Result<(), String> {
    if l.partitions.is_empty() {
        return Ok(());
    }
    for p in d.partitions.iter().filter(|p| p.is_system()) {
        if !l.partitions.iter().any(|q| q.offset == p.offset && q.length >= p.length) {
            return Err(format!("no partition matching {} at the same position", p.label));
        }
    }
    Ok(())
}
