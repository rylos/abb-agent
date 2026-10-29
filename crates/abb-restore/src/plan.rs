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
    /// None until a target is chosen.
    pub place: Option<Place>,
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
    /// btrfs subvolumes to leave out (patterns from the top level, `*` and `?`): their data is
    /// not downloaded, their snapshots and nested subvolumes go with them, and they are
    /// recreated empty.
    pub exclude_subvolumes: Vec<String>,
}

/// End of the last partition of a disk of the backup (bytes).
pub fn used_end(d: &Disk) -> u64 {
    d.partitions.iter().map(|p| p.offset + p.length).max().unwrap_or(1 << 20)
}

/// Can disk `d` of the backup go on local disk `l`?
pub fn disk_fits(d: &Disk, l: &LocalDisk) -> Result<(), String> {
    if let Some(why) = &l.busy {
        return Err(why.clone());
    }
    if d.gpt && l.sector_size != d.sector_size {
        return Err(format!("sector size {} instead of {}", l.sector_size, d.sector_size));
    }
    // The write stops at the end of the local disk; the backup table needs its room after the
    // partitions (the entry array is made smaller if it has to: `Gpt::fit`).
    let need = if d.gpt { used_end(d) + Gpt::tail(d.sector_size) } else { d.size };
    if l.size < need {
        return Err(format!("too small: {} needed", size_str(need)));
    }
    Ok(())
}

/// Picks local disks for the disks of the backup: the smallest that fits, biggest disks first.
pub fn auto_map(v: &Version, locals: &[LocalDisk]) -> Vec<Option<usize>> {
    map_disks(v, locals, |_| true)
}

/// [`auto_map`] for system mode: only the disks that hold system volumes.
pub fn system_map(v: &Version, locals: &[LocalDisk]) -> Vec<Option<usize>> {
    map_disks(v, locals, |d| d.partitions.iter().any(Partition::is_system))
}

fn map_disks(v: &Version, locals: &[LocalDisk], want: impl Fn(&Disk) -> bool) -> Vec<Option<usize>> {
    let mut order: Vec<usize> = (0..v.disks.len()).filter(|&i| want(&v.disks[i])).collect();
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

/// Why no new partition can be made on local disk `l`, if so. Without a GPT the disk must be
/// blank: an MBR table or a filesystem there would be overwritten by the new table.
pub fn free_refused(l: &LocalDisk) -> Option<String> {
    if l.gpt.is_some() {
        None
    } else if !l.partitions.is_empty() {
        Some("partitions but no GPT (MBR?): new partitions only on a GPT disk or a blank one".into())
    } else {
        l.content.map(|c| format!("no GPT but {c}: new partitions only on a GPT disk or a blank one"))
    }
}

/// `areas` without the sectors `start..=end`; what is left after them starts aligned.
fn cut(areas: Vec<(u64, u64)>, start: u64, end: u64, align: u64) -> Vec<(u64, u64)> {
    let mut next = Vec::new();
    for (a, b) in areas {
        if end < a || start > b {
            next.push((a, b));
            continue;
        }
        if start > a {
            next.push((a, start - 1));
        }
        let after = end.saturating_add(1).div_ceil(align) * align;
        if after <= b {
            next.push((after, b));
        }
    }
    next
}

/// Free areas (first, last sector) of local disk `l`, minus the new partitions of `taken`. None
/// if new partitions cannot be made there ([`free_refused`]).
pub fn free_areas(v: &Version, l: &LocalDisk, local: usize, taken: &[VolumeTarget]) -> Vec<(u64, u64)> {
    if free_refused(l).is_some() {
        return vec![];
    }
    let align = MIB / l.sector_size;
    let mut areas = l.gpt.clone().unwrap_or_else(|| Gpt::new(l.sector_size)).free(l.size);
    // The partitions the kernel shows too, should they differ from the table.
    for q in l.partitions.iter().filter(|q| q.length > 0) {
        areas = cut(areas, q.offset / l.sector_size, (q.offset + q.length).div_ceil(l.sector_size) - 1, align);
    }
    for t in taken.iter().filter(|t| t.local == local) {
        let Some(Place::Free(start)) = t.place else { continue };
        let end = start + v.disks[t.disk].partitions[t.part].length.div_ceil(l.sector_size).max(1) - 1;
        areas = cut(areas, start, end, align);
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
            if let Some(why) = l.busy.clone().or_else(|| free_refused(l)) {
                return Err(why);
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
                    let Some(place) = &t.place else {
                        errs.push(format!("{}: no target chosen", p.label));
                        continue;
                    };
                    let free = free_areas(v, &locals[t.local], t.local, &self.volumes[..k]);
                    if let Err(e) = place_room(p, &locals[t.local], place, &free) {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::local::LocalPartition;

    const GIB: u64 = 1 << 30;

    pub(crate) fn part(number: u64, offset: u64, length: u64, mount: &str, attribute: u64) -> Partition {
        Partition {
            number,
            offset,
            length,
            label: mount.into(),
            mount_path: mount.into(),
            device: String::new(),
            fs: "ext4".into(),
            attribute,
            excluded: false,
        }
    }

    /// A backup with a system disk (ESP and /) and a data disk.
    pub(crate) fn version() -> Version {
        let sys = Disk {
            number: 0,
            image_path: "v/sda.img".into(),
            object_id: 1,
            size: 20 * GIB,
            sector_size: 512,
            gpt: true,
            partitions: vec![part(1, MIB, 512 * MIB, "/boot/efi", 9), part(2, 513 * MIB, 10 * GIB, "/", 0)],
        };
        let data = Disk {
            number: 1,
            image_path: "v/sdb.img".into(),
            object_id: 2,
            size: 40 * GIB,
            sector_size: 512,
            gpt: true,
            partitions: vec![part(1, MIB, 30 * GIB, "/data", 0)],
        };
        Version { id: 7, time: 0, bootable: true, disks: vec![sys, data] }
    }

    pub(crate) fn local(name: &str, size: u64) -> LocalDisk {
        LocalDisk {
            name: name.into(),
            size,
            sector_size: 512,
            model: String::new(),
            removable: false,
            busy: None,
            partitions: vec![],
            gpt: None,
            content: None,
            rdev: 0,
            id: String::new(),
        }
    }

    fn lpart(number: u64, offset: u64, length: u64) -> LocalPartition {
        LocalPartition { number, name: format!("sdx{number}"), offset, length, busy: None }
    }

    #[test]
    fn mbr_disk_is_not_free() {
        let v = version();
        let mut l = local("sdx", 100 * GIB);
        l.partitions = vec![lpart(1, MIB, 50 * GIB)];
        assert!(free_refused(&l).is_some());
        assert!(free_areas(&v, &l, 0, &[]).is_empty());
        assert_eq!(first_fit(&v, std::slice::from_ref(&l), 0, 1, 0, &[]), None);
        let err = place_room(&v.disks[1].partitions[0], &l, &Place::Free(2048), &[(2048, 1 << 30)]).unwrap_err();
        assert!(err.contains("no GPT"), "{err}");
        // No partition the kernel knows of, but an MBR (or a filesystem) at the start.
        l.partitions.clear();
        l.content = Some("an MBR partition table or a boot sector");
        assert!(free_refused(&l).is_some());
        assert!(free_areas(&v, &l, 0, &[]).is_empty());
        // Blank: free from the first MiB on.
        l.content = None;
        assert_eq!(free_refused(&l), None);
        assert_eq!(free_areas(&v, &l, 0, &[])[0].0, 2048);
    }

    #[test]
    fn free_areas_leave_out_known_partitions() {
        let v = version();
        let mut l = local("sdx", 100 * GIB);
        // An empty GPT, while the kernel shows a partition (a table it read differently).
        l.gpt = Some(Gpt::new(512));
        l.partitions = vec![lpart(1, MIB, GIB)];
        let areas = free_areas(&v, &l, 0, &[]);
        assert_eq!(areas[0].0, (GIB + MIB) / 512);
        // A volume placed there takes its room; the next area starts aligned after it.
        let taken = [VolumeTarget { disk: 1, part: 0, local: 0, place: Some(Place::Free(areas[0].0)) }];
        let after = free_areas(&v, &l, 0, &taken);
        assert_eq!(after[0].0, areas[0].0 + 30 * GIB / 512);
    }

    #[test]
    fn disk_fits_room_for_the_backup_table() {
        let v = version();
        let d = &v.disks[0];
        let end = used_end(d);
        assert!(disk_fits(d, &local("sdx", end + 33 * 512)).is_ok());
        assert!(disk_fits(d, &local("sdx", end + 32 * 512)).is_err());
        // 4 KiB sectors: a 16 KiB array is 4 sectors.
        let mut d4 = d.clone();
        d4.sector_size = 4096;
        let mut l4 = local("sdx", end + 5 * 4096);
        l4.sector_size = 4096;
        assert!(disk_fits(&d4, &l4).is_ok());
        l4.size -= 4096;
        assert!(disk_fits(&d4, &l4).is_err());
        // Sector sizes must match for a GPT disk.
        assert!(disk_fits(d, &l4).is_err());
    }

    #[test]
    fn system_map_skips_data_disks() {
        let v = version();
        let locals = [local("sdx", 50 * GIB), local("sdy", 50 * GIB)];
        assert_eq!(auto_map(&v, &locals).iter().filter(|m| m.is_some()).count(), 2);
        assert_eq!(system_map(&v, &locals)[1], None);
        assert!(system_map(&v, &locals)[0].is_some());
    }

    #[test]
    fn no_target_fails_the_check() {
        let v = version();
        let locals = [local("sdx", 100 * GIB)];
        let mut plan = Plan {
            mode: Mode::Volumes,
            disks: vec![None; 2],
            volumes: vec![VolumeTarget { disk: 1, part: 0, local: 0, place: None }],
            grow: false,
            efi_fallback: false,
            exclude_subvolumes: vec![],
        };
        assert_eq!(plan.check(&v, &locals), vec!["/data: no target chosen".to_string()]);
        plan.volumes[0].place = first_fit(&v, &locals, 0, 1, 0, &[]).map(Place::Free);
        assert!(plan.check(&v, &locals).is_empty());
    }
}
