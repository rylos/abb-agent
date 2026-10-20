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

use crate::restore::{Device, Event, Job, Special, download};
use crate::server::{Range, Server};
use crate::spec::{Task, Version};
use btrfs_image::Filesystem;
use btrfs_image::disk::{BLOCK_GROUP_DATA, BLOCK_GROUP_METADATA, BLOCK_GROUP_SYSTEM, Chunk, SUPER_OFFSET, SUPER_SIZE, Superblock};
use btrfs_image::usedmap::{Kind, UsedMap};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

const MIB: u64 = 1 << 20;
/// Gaps up to this size between data ranges are downloaded rather than split: the server skips
/// what is unused anyway, and fewer ranges keep the task small.
const MERGE_GAP: u64 = 16 * MIB;
const MAX_RANGES: usize = 2000;

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

fn fetch(ctx: &mut Ctx, sp: &Special, pieces: &[Piece], what: &str, report: &mut dyn FnMut(Event)) -> Result<(), String> {
    if pieces.is_empty() {
        return Ok(());
    }
    let d = &ctx.version.disks[sp.disk];
    let p = &sp.part;
    let mut jobs: Vec<Job> = pieces
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
    let ranges: Vec<Range> = jobs.iter().map(|j| j.range.clone()).collect();
    let total = ranges.iter().map(|r| r.length).sum();
    report(Event::Step(format!("{}: {what}", p.label)));
    let running = ctx
        .server
        .start(ctx.task.device_id, ctx.task.task_id, ctx.version.id, ctx.mode, &crate::local::machine_uuid(), &ranges)
        .map_err(|e| format!("the server refused the restore: {e}"))?;
    let res = download(ctx.server, running, &mut jobs, ctx.devices, total, ctx.cancel, report);
    let status = if res.is_ok() { ("complete", 1203) } else { ("error", 1000) };
    let _ = ctx.server.finish(running, status.0, status.1);
    res?;
    ctx.devices[sp.dev].file.sync_all().map_err(|e| e.to_string())
}

/// Pieces covering the chunks of `kind`: the first stripe downloaded, the others as mirrors.
fn chunk_pieces<'a>(chunks: impl Iterator<Item = &'a Chunk>, kind: impl Fn(u64) -> bool) -> Vec<Piece> {
    chunks
        .filter(|c| kind(c.kind))
        .map(|c| Piece { offset: c.stripes[0], length: c.length, mirrors: c.stripes[1..].to_vec() })
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

    // 1. Superblock, then the system chunks.
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
    fetch(&mut ctx, sp, &chunk_pieces(sys.iter(), |_| true), "chunk tree", report)?;

    // 2. All metadata.
    let fs = Filesystem::open_at(&dev.path, base).map_err(err)?;
    let meta = chunk_pieces(fs.chunks.values(), |k| k & (BLOCK_GROUP_METADATA | BLOCK_GROUP_SYSTEM) != 0 && k & BLOCK_GROUP_SYSTEM == 0);
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
                    dev.file.write_all_at(node, base + phys).map_err(|e| e.to_string())?;
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
    let mut want: Vec<(u64, u64)> = map
        .ranges
        .iter()
        .filter(|r| match r.kind {
            Kind::Data => skip.binary_search_by_key(&r.logical, |d| d.0).is_err(),
            Kind::Reserved => r.physical >= MIB,
            Kind::Tree => false,
        })
        .filter(|r| !mixed.iter().any(|c| c.stripes.iter().any(|&s| r.physical >= s && r.physical < s + c.length)))
        .map(|r| (r.physical, r.len))
        .collect();
    want.sort();
    let mut gap = MERGE_GAP;
    let pieces = loop {
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for &(o, l) in &want {
            match merged.last_mut() {
                Some((mo, ml)) if o <= *mo + *ml + gap => *ml = (o + l).max(*mo + *ml) - *mo,
                _ => merged.push((o, l)),
            }
        }
        if merged.len() <= MAX_RANGES {
            break merged;
        }
        gap *= 4;
    };
    drop(fs);
    let pieces: Vec<Piece> = pieces.into_iter().map(|(offset, length)| Piece { offset, length, mirrors: vec![] }).collect();
    let what = if skipped > 0 {
        format!("data ({} left out)", crate::local::size_str(skipped))
    } else {
        "data".to_string()
    };
    fetch(&mut ctx, sp, &pieces, &what, report)?;
    Ok(left)
}
