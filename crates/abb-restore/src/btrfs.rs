//! Restoring a btrfs partition without some of its subvolumes.
//!
//! The server reads any byte range of an image, so the partition comes down in three passes,
//! each one a restore task on the server:
//! 1. the first MiB (primary superblock), then the system chunks it lists (the chunk tree);
//! 2. every metadata chunk: the whole filesystem without file data;
//! 3. the data the remaining subvolumes use, once the excluded ones have been deleted in the
//!    restored metadata exactly as the agent does at backup time (`btrfs_image::exclude`).
//!
//! Metadata chunks are DUP on a single device: one copy is downloaded and written to both.

use crate::restore::{Device, Event, Job, Special, transfer};
use crate::server::{Range, Server};
use crate::spec::{Task, Version};
use btrfs_image::Filesystem;
use btrfs_image::disk::{BLOCK_GROUP_DATA, BLOCK_GROUP_METADATA, BLOCK_GROUP_SYSTEM, Chunk, SUPER_OFFSET, SUPER_SIZE, Superblock};
use btrfs_image::usedmap::{Kind, UsedMap};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

const MIB: u64 = 1 << 20;
/// Used bytes not wanted that may come down between two wanted ranges to make them one. Measured
/// on a NAS: wider merges are no faster (the server sets the pace) and send more stale data.
const MERGE_WASTE: u64 = MIB;

pub struct Ctx<'a> {
    pub server: &'a mut Server,
    pub task: &'a Task,
    pub version: &'a Version,
    pub mode: u64,
    pub devices: &'a [Device],
    pub cancel: &'a AtomicBool,
}

/// A range of the partition (offset from its start) and the extra copies to write.
struct Piece {
    offset: u64,
    length: u64,
    mirrors: Vec<u64>,
}

/// Offsets come from the image, that is from the server: nothing may be written outside the
/// partition.
fn inside(offset: u64, length: u64, part_len: u64) -> bool {
    offset.checked_add(length).is_some_and(|end| end <= part_len)
}

fn check_pieces(pieces: &[Piece], part_len: u64) -> Result<(), String> {
    for p in pieces {
        for o in std::iter::once(p.offset).chain(p.mirrors.iter().copied()) {
            if !inside(o, p.length, part_len) {
                return Err(format!("{} bytes at {o} are outside the partition ({part_len} bytes)", p.length));
            }
        }
    }
    Ok(())
}

fn fetch(ctx: &mut Ctx, sp: &Special, pieces: &[Piece], what: &str, report: &mut dyn FnMut(Event)) -> Result<(), String> {
    if pieces.is_empty() {
        return Ok(());
    }
    check_pieces(pieces, sp.part.length).map_err(|e| format!("{}: {what}: {e}", sp.part.label))?;
    let d = &ctx.version.disks[sp.disk];
    let p = &sp.part;
    let jobs: Vec<Job> = pieces
        .iter()
        .map(|x| Job {
            range: Range {
                image_path: d.image_path.clone(),
                object_id: d.object_id,
                disk_number: d.number + 1,
                partition_number: p.number,
                alias_name: p.label.clone(),
                offset: p.offset + x.offset,
                length: x.length,
                done: 0,
            },
            dev: Some(sp.dev),
            delta: sp.delta,
            table: None,
            mirrors: x.mirrors.iter().map(|&m| sp.delta + m as i64 - x.offset as i64).collect(),
        })
        .collect();
    report(Event::Step(format!("{}: {what}", p.label)));
    let (task, version, mode) = (ctx.task, ctx.version, ctx.mode);
    let start = |s: &mut Server, ranges: &[Range]| s.start(task.device_id, task.task_id, version.id, mode, &crate::local::machine_uuid(), ranges);
    transfer(ctx.server, &start, jobs, ctx.devices, ctx.cancel, report)?;
    let dev = &ctx.devices[sp.dev];
    dev.file.sync_all().map_err(|e| format!("disk: {}: {e}", dev.path.display()))
}

/// Pieces covering the chunks of `kind`: the first stripe downloaded, the others as mirrors.
fn chunk_pieces<'a>(chunks: impl Iterator<Item = &'a Chunk>, kind: impl Fn(u64) -> bool) -> Result<Vec<Piece>, String> {
    chunks
        .filter(|c| kind(c.kind))
        .map(|c| match c.stripes.split_first() {
            Some((&first, rest)) => Ok(Piece { offset: first, length: c.length, mirrors: rest.to_vec() }),
            None => Err(format!("chunk at {} without a stripe", c.logical)),
        })
        .collect()
}

/// What is left to do on the mounted filesystem.
#[derive(Default)]
pub struct Left {
    /// Subvolumes to recreate empty (those matched directly).
    pub recreate: Vec<PathBuf>,
    /// Ids of the deleted subvolumes, to wait for btrfs to drop them.
    pub deleted: Vec<u64>,
}

/// Restores partition `sp` leaving out the subvolumes matching `patterns`.
pub fn restore(mut ctx: Ctx, sp: &Special, patterns: &[String], report: &mut dyn FnMut(Event)) -> Result<Left, String> {
    let dev = &ctx.devices[sp.dev];
    let base = (sp.part.offset as i64 + sp.delta) as u64;
    let err = |e: btrfs_image::Error| format!("{}: {e}", sp.part.label);
    let label = |e: String| format!("{}: {e}", sp.part.label);

    // 1. Superblock, then the system chunks.
    if sp.part.length < MIB {
        return Err(label("too small for btrfs".into()));
    }
    fetch(&mut ctx, sp, &[Piece { offset: 0, length: MIB, mirrors: vec![] }], "superblock", report)?;
    let mut raw = vec![0u8; SUPER_SIZE];
    dev.file.read_exact_at(&mut raw, base + SUPER_OFFSET).map_err(|e| e.to_string())?;
    let sb = Superblock::parse(raw).map_err(err)?;
    let arr = sb.sys_chunk_array().to_vec();
    let mut sys = Vec::new();
    let mut o = 0;
    while o + 17 <= arr.len() {
        o += 17;
        let logical = u64::from_le_bytes(arr[o - 8..o].try_into().unwrap());
        let (c, size) = Chunk::parse(logical, &arr[o..], sb.devid).map_err(err)?;
        o += size;
        sys.push(c);
    }
    fetch(&mut ctx, sp, &chunk_pieces(sys.iter(), |_| true).map_err(label)?, "chunk tree", report)?;

    // 2. All metadata.
    let fs = Filesystem::open_at(&dev.path, base).map_err(err)?;
    let meta = chunk_pieces(fs.chunks.values(), |k| k & (BLOCK_GROUP_METADATA | BLOCK_GROUP_SYSTEM) != 0 && k & BLOCK_GROUP_SYSTEM == 0).map_err(label)?;
    let meta_size: u64 = meta.iter().map(|p| p.length).sum();
    fetch(&mut ctx, sp, &meta, &format!("metadata ({})", crate::local::size_str(meta_size)), report)?;

    // 3. Leave the subvolumes out in the restored metadata, then download the data still used.
    let fs = Filesystem::open_at(&dev.path, base).map_err(err)?;
    let plan = btrfs_image::exclude::restore_plan(&fs, patterns).map_err(err)?;
    let map = UsedMap::build(&fs).map_err(err)?;
    let (skip, left) = match &plan {
        Some((ex, picked)) => {
            report(Event::Step(format!(
                "{}: leaving out {}",
                sp.part.label,
                ex.subvolumes.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            )));
            for (logical, node) in &ex.nodes {
                for phys in fs.physical(*logical).map_err(err)? {
                    if !inside(phys, node.len() as u64, sp.part.length) {
                        return Err(label(format!("tree node at {phys} outside the partition")));
                    }
                    dev.file.write_all_at(node, base + phys).map_err(|e| format!("disk: {}: {e}", dev.path.display()))?;
                }
            }
            (ex.data.clone(), Left { recreate: picked.clone(), deleted: ex.ids.clone() })
        }
        None => {
            report(Event::Warning(format!("{}: no subvolume matches {}: restoring all of it", sp.part.label, patterns.join(", "))));
            (vec![], Left::default())
        }
    };
    let skipped: u64 = skip.iter().map(|d| d.1).sum();
    // Data extents in use and not left out, plus the superblock copies; mixed block groups
    // (data and metadata together) came down with the metadata.
    let mixed: Vec<&Chunk> = fs.chunks.values().filter(|c| c.kind & BLOCK_GROUP_DATA != 0 && c.kind & BLOCK_GROUP_METADATA != 0).collect();
    let used: Vec<(u64, u64, bool)> = map
        .ranges
        .iter()
        .map(|r| {
            let wanted = match r.kind {
                Kind::Data => skip.binary_search_by_key(&r.logical, |d| d.0).is_err(),
                Kind::Reserved => r.physical >= MIB,
                Kind::Tree => false,
            } && !mixed.iter().any(|c| c.stripes.iter().any(|&s| r.physical >= s && r.physical < s + c.length));
            (r.physical, r.len, wanted)
        })
        .collect();
    drop(fs);
    let pieces = join(used, MERGE_WASTE);
    let wanted: u64 = pieces.iter().map(|p| p.2).sum();
    let requested: u64 = pieces.iter().map(|p| p.1).sum();
    let pieces: Vec<Piece> = pieces.into_iter().map(|(offset, length, _)| Piece { offset, length, mirrors: vec![] }).collect();
    let what = if skipped > 0 {
        format!(
            "data ({} left out; {} in use, {} requested in {} ranges)",
            crate::local::size_str(skipped),
            crate::local::size_str(wanted),
            crate::local::size_str(requested),
            pieces.len()
        )
    } else {
        "data".to_string()
    };
    fetch(&mut ctx, sp, &pieces, &what, report)?;
    Ok(left)
}

/// Joins the wanted ranges of `used` (physical offset, length, wanted) into pieces to request:
/// (offset, length, wanted bytes inside). Two wanted ranges become one piece when what lies
/// between them costs at most `waste`: used ranges not wanted count, free space does not, as the
/// server sends only the parts of it that the image holds.
fn join(mut used: Vec<(u64, u64, bool)>, waste: u64) -> Vec<(u64, u64, u64)> {
    used.sort_unstable();
    let mut out: Vec<(u64, u64, u64)> = Vec::new();
    let mut between = 0;
    for (o, l, wanted) in used {
        if !wanted {
            between += l;
            continue;
        }
        match out.last_mut() {
            Some((po, pl, pw)) if between <= waste => {
                *pl = (o + l).max(*po + *pl) - *po;
                *pw += l;
            }
            _ => out.push((o, l, l)),
        }
        between = 0;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_inside_the_partition() {
        let piece = |offset, length, mirrors: Vec<u64>| Piece { offset, length, mirrors };
        assert!(check_pieces(&[piece(0, 100, vec![100])], 200).is_ok());
        assert!(check_pieces(&[piece(150, 100, vec![])], 200).is_err());
        assert!(check_pieces(&[piece(0, 100, vec![150])], 200).is_err());
        assert!(check_pieces(&[piece(u64::MAX, 2, vec![])], u64::MAX).is_err());
        let chunk = |stripes: Vec<u64>| Chunk { logical: 0, length: 10, kind: BLOCK_GROUP_METADATA, stripes };
        let pieces = chunk_pieces([chunk(vec![5, 50])].iter(), |_| true).unwrap();
        assert_eq!((pieces[0].offset, pieces[0].mirrors.clone()), (5, vec![50]));
        assert!(chunk_pieces([chunk(vec![])].iter(), |_| true).is_err());
    }

    #[test]
    fn joining() {
        // Wanted 0..10 and 20..30 with 10 bytes not wanted between: one piece only when the
        // waste allows it. Free space (30..100) costs nothing: 20..30 and 100..105 always join.
        let used = vec![(20, 10, true), (10, 10, false), (0, 10, true), (100, 5, true)];
        assert_eq!(join(used.clone(), 0), vec![(0, 10, 10), (20, 85, 15)]);
        assert_eq!(join(used, 10), vec![(0, 105, 25)]);
        // A wanted range inside another one does not shrink the piece.
        assert_eq!(join(vec![(0, 100, true), (10, 5, true)], 0)[0].1, 100);
    }
}
