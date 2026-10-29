//! Leaving subvolumes out of the image.
//!
//! An excluded subvolume is turned, in the image only, into what `btrfs subvolume delete`
//! leaves behind before the cleaner runs: its root item gets zero references and the "dead"
//! flag, an orphan item queues it for deletion, and its root ref, back ref and directory entry
//! are removed. Its tree is still sent (it is metadata, and the cleaner reads it), but data
//! extents referenced only by excluded subvolumes are not: they are marked unused. When the
//! restored filesystem is first mounted, the kernel drops the subvolume and frees those
//! extents, as after any subvolume deletion.
//!
//! Nested subvolumes go with their parent. Snapshots of an excluded subvolume are excluded
//! only if they match a pattern themselves; until they are, the data they share stays.
//!
//! The directory entry is removed from nodes shared with the agent's snapshots of the parent,
//! which get the same edit; if another snapshot (snapper, btrbk) shares them, the exclusion is
//! refused and everything is backed up (see `unlink`).

use crate::capture::Pinned;
use crate::disk::*;
use crate::edit::{self, Editor};
use crate::snapshot::Subvolume;
use crate::{Error, Filesystem};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const INODE_ITEM_KEY: u8 = 1;
const DIR_ITEM_KEY: u8 = 84;
const DIR_INDEX_KEY: u8 = 96;
const EXTENT_DATA_KEY: u8 = 108;
const ORPHAN_ITEM_KEY: u8 = 48;
const ROOT_BACKREF_KEY: u8 = 144;
const ROOT_REF_KEY: u8 = 156;
const ORPHAN_OBJECTID: u64 = u64::MAX - 4;
const FS_TREE_OBJECTID: u64 = 5;
const FIRST_FREE_OBJECTID: u64 = 256;
const ROOT_SUBVOL_DEAD: u64 = 1 << 48;

pub struct Exclusion {
    /// Excluded subvolumes (paths from the top level).
    pub subvolumes: Vec<PathBuf>,
    /// Their ids, in the same order.
    pub ids: Vec<u64>,
    /// Edited tree nodes, by logical address.
    pub nodes: HashMap<u64, Vec<u8>>,
    /// Data extents not sent: (logical, length), sorted.
    pub data: Vec<(u64, u64)>,
}

/// `*` matches any sequence of characters, `?` one character.
pub fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// btrfs directory name hash: crc32c with seed ~1, without the final inversion.
fn name_hash(name: &[u8]) -> u64 {
    let mut crc: u32 = !1;
    for &b in name {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82F6_3B78 & (crc & 1).wrapping_neg());
        }
    }
    crc as u64
}

/// Data extents (logical start) referenced by the file trees in `roots`. Nodes shared between
/// trees (snapshots) are visited once.
fn referenced_data(fs: &Filesystem, roots: &[(u64, u64)]) -> Result<HashSet<u64>, Error> {
    let mut out = HashSet::new();
    fs.walk_parallel(roots, &mut |node| {
        if node.header.level > 0 {
            return Ok(node.ptrs().map(|(_, child, g)| (child, g)).collect());
        }
        for (key, data) in node.items() {
            // file_extent_item: type at 20; for regular and prealloc extents disk_bytenr at 21.
            if key.kind == EXTENT_DATA_KEY && data.len() >= 29 && data[20] != 0 {
                let disk = u64_at(data, 21);
                if disk != 0 {
                    out.insert(disk);
                }
            }
        }
        Ok(Vec::new())
    })?;
    Ok(out)
}

/// Subvolumes matching `patterns`, with those nested in them (the agent's snapshots in
/// `snap_dir` never match).
pub fn matching(subvols: &[Subvolume], snap_dir: &Path, patterns: &[String]) -> Vec<Subvolume> {
    let mut ids: HashSet<u64> = subvols
        .iter()
        .filter(|s| !s.path.starts_with(snap_dir))
        .filter(|s| patterns.iter().any(|p| glob(p.trim_matches('/'), &s.path.to_string_lossy())))
        .map(|s| s.id)
        .collect();
    loop {
        let before = ids.len();
        for s in subvols {
            if ids.contains(&s.parent) {
                ids.insert(s.id);
            }
        }
        if ids.len() == before {
            break;
        }
    }
    let mut out: Vec<Subvolume> = subvols.iter().filter(|s| ids.contains(&s.id)).cloned().collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Works out the subvolumes matching `patterns` (and those nested in them, and the agent's own
/// snapshots of them), the data only they reference, and the edits that delete them in the
/// image. `subvols` are the subvolumes at the imaged generation, `pinned` the agent's snapshots
/// (`snapshots[i]` is the snapshot subvolume of `pinned[i]`).
pub fn plan(
    fs: &Filesystem,
    subvols: &[Subvolume],
    pinned: &[Pinned],
    snapshots: &[u64],
    snap_dir: &Path,
    patterns: &[String],
    data_extents: impl Iterator<Item = (u64, u64)>,
) -> Result<Option<Exclusion>, Error> {
    let excluded: HashSet<u64> = matching(subvols, snap_dir, patterns).iter().map(|s| s.id).collect();
    plan_ids(fs, subvols, pinned, snapshots, excluded, data_extents)
}

/// [`plan`] for the subvolumes `excluded`, by id.
fn plan_ids(
    fs: &Filesystem,
    subvols: &[Subvolume],
    pinned: &[Pinned],
    snapshots: &[u64],
    mut excluded: HashSet<u64>,
    data_extents: impl Iterator<Item = (u64, u64)>,
) -> Result<Option<Exclusion>, Error> {
    if excluded.is_empty() {
        return Ok(None);
    }
    // Nested subvolumes, and the agent's snapshots of everything excluded.
    loop {
        let before = excluded.len();
        for s in subvols {
            if excluded.contains(&s.parent) {
                excluded.insert(s.id);
            }
        }
        for (p, snap) in pinned.iter().zip(snapshots) {
            if excluded.contains(&p.subvol) {
                excluded.insert(*snap);
            }
        }
        if excluded.len() == before {
            break;
        }
    }

    let items = fs.root_items()?;
    let root_of = |id: u64| items.iter().find(|(i, _)| *i == id).map(|(_, d)| (u64_at(d, 176), u64_at(d, 160)));

    // Data referenced by anything that stays: the root tree (v1 free space cache), the top level
    // and every other file tree. Dead trees do not stay: the cleaner drops them at mount, and
    // the ones left out at backup time never had their own data sent.
    let mut kept_roots = vec![(fs.sb.root, 0)];
    for (id, d) in &items {
        let dead = u64_at(d, 208) & ROOT_SUBVOL_DEAD != 0;
        if (*id == FS_TREE_OBJECTID || *id >= FIRST_FREE_OBJECTID) && !excluded.contains(id) && !dead {
            kept_roots.push((u64_at(d, 176), u64_at(d, 160)));
        }
    }
    let kept = referenced_data(fs, &kept_roots)?;
    let mut data: Vec<(u64, u64)> = data_extents.filter(|(l, _)| !kept.contains(l)).collect();
    data.sort();
    data.dedup();

    let mut ed = Editor::new(fs);
    let root = fs.sb.root;
    let (extent_root, _) = fs.tree_root(EXTENT_TREE_OBJECTID)?;
    let mut ids: Vec<u64> = excluded.iter().copied().collect();
    ids.sort();
    for &id in &ids {
        let (rkey, _) = ed
            .find_first(root, id, ROOT_ITEM_KEY)?
            .ok_or_else(|| Error::Format(format!("root item of subvolume {id} not found")))?;
        ed.update(root, rkey, |d| {
            d[216..220].copy_from_slice(&0u32.to_le_bytes());
            let flags = u64_at(d, 208) | ROOT_SUBVOL_DEAD;
            d[208..216].copy_from_slice(&flags.to_le_bytes());
        })?;
        ed.insert(root, Key { objectid: ORPHAN_OBJECTID, kind: ORPHAN_ITEM_KEY, offset: id }, &[])?;
        let Some((bkey, _)) = ed.find_first(root, id, ROOT_BACKREF_KEY)? else { continue };
        let parent = bkey.offset;
        ed.delete(root, bkey)?;
        let rref_key = Key { objectid: parent, kind: ROOT_REF_KEY, offset: id };
        let rref = ed.get(root, rref_key)?.ok_or_else(|| Error::Format(format!("root ref {parent} → {id} not found")))?;
        ed.delete(root, rref_key)?;
        if excluded.contains(&parent) {
            // The directory entry is inside a tree that goes away too.
            continue;
        }
        let (dirid, index, name) = root_ref(&rref)?;
        let name = name.to_vec();
        let (ptree, _) = root_of(parent).ok_or_else(|| Error::Format(format!("root of subvolume {parent} not found")))?;
        // The agent's snapshots of the parent share its nodes: they get the same edit.
        let copies: Vec<u64> = pinned
            .iter()
            .zip(snapshots)
            .filter(|(p, s)| p.subvol == parent && !excluded.contains(*s))
            .filter_map(|(_, s)| root_of(*s).map(|r| r.0))
            .collect();
        unlink(&mut ed, extent_root, ptree, &copies, dirid, index, &name)
            .map_err(|e| Error::Unsupported(format!("subvolume {id} ({}): {e}", String::from_utf8_lossy(&name))))?;
    }

    let mut gone: Vec<&Subvolume> = subvols.iter().filter(|s| excluded.contains(&s.id)).collect();
    gone.sort_by(|a, b| a.path.cmp(&b.path));
    let (subvolumes, ids) = gone.iter().map(|s| (s.path.clone(), s.id)).unzip();
    Ok(Some(Exclusion { subvolumes, ids, nodes: ed.finish(), data }))
}

/// References to a tree block in the extent tree: parent nodes and roots pointing to it.
fn block_refs(ed: &Editor, extent_root: u64, node: u64) -> Result<u64, Error> {
    for kind in [METADATA_ITEM_KEY, EXTENT_ITEM_KEY] {
        if let Some((_, item)) = ed.find_first(extent_root, node, kind)?
            && item.len() >= 8
        {
            return Ok(u64_at(&item, 0));
        }
    }
    Err(Error::Format(format!("tree block {node} not in the extent tree")))
}

/// The first node of `paths` (from tree roots, each referenced by its root item only) that more
/// parents reference than those on the paths: some other tree reaches it.
fn shared_node(paths: &[&edit::Path], refs: &mut dyn FnMut(u64) -> Result<u64, Error>) -> Result<Option<u64>, Error> {
    let mut parents: HashMap<u64, HashSet<u64>> = HashMap::new();
    for p in paths {
        for (i, &(node, _)) in p.iter().enumerate() {
            // 0 stands for the root item.
            parents.entry(node).or_default().insert(if i == 0 { 0 } else { p[i - 1].0 });
        }
    }
    let mut nodes: Vec<(u64, usize)> = parents.into_iter().map(|(n, p)| (n, p.len())).collect();
    nodes.sort_unstable();
    for (node, n) in nodes {
        if refs(node)? > n as u64 {
            return Ok(Some(node));
        }
    }
    Ok(None)
}

/// Removes the entry `name` from a directory item (several names can share a hash): the
/// remaining entries, or None if `name` is not there.
fn without_entry(ditem: &[u8], name: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    let mut rest = Vec::new();
    let mut o = 0;
    let mut removed = false;
    while o + 30 <= ditem.len() {
        let (dlen, elen) = (u16_at(ditem, o + 25) as usize, u16_at(ditem, o + 27) as usize);
        let end = o + 30 + elen + dlen;
        if end > ditem.len() {
            return Err(Error::Format("truncated directory item".into()));
        }
        if !removed && ditem[o + 30..o + 30 + elen] == name[..] {
            removed = true;
        } else {
            rest.extend_from_slice(&ditem[o..end]);
        }
        o = end;
    }
    Ok(removed.then_some(rest))
}

/// Removes the directory entry of a subvolume (`name`, index `index` in directory `dirid`) from
/// file tree `tree` and from `copies`, the agent's snapshots of that tree.
///
/// The edits are made on nodes as they are, not on copies (see `edit`): every tree reaching an
/// edited node sees the edit. `tree` and the agent's snapshots of it get the same edit, and the
/// key pointers of their own nodes are repaired. Any other tree (snapper's or btrbk's snapshots
/// sharing the directory) would see its directory change under it, and a pointer of its own to
/// a node whose first key changed would no longer match (read errors, failed snapshot deletion):
/// if one reaches any node involved, the exclusion is refused.
fn unlink(ed: &mut Editor, extent_root: u64, tree: u64, copies: &[u64], dirid: u64, index: u64, name: &[u8]) -> Result<(), Error> {
    let keys = [
        Key { objectid: dirid, kind: DIR_INDEX_KEY, offset: index },
        Key { objectid: dirid, kind: DIR_ITEM_KEY, offset: name_hash(name) },
        Key { objectid: dirid, kind: INODE_ITEM_KEY, offset: 0 },
    ];
    let roots: Vec<u64> = std::iter::once(tree).chain(copies.iter().copied()).collect();
    let mut paths: Vec<Vec<edit::Path>> = Vec::new();
    for &r in &roots {
        paths.push(keys.iter().map(|&k| ed.path(r, k)).collect::<Result<_, _>>()?);
    }
    let all: Vec<&edit::Path> = paths.iter().flatten().collect();
    if let Some(node) = shared_node(&all, &mut |n| block_refs(ed, extent_root, n))? {
        return Err(Error::Unsupported(format!("node {node} of its directory is shared with another snapshot")));
    }
    let leaf = |p: &edit::Path| p.last().map(|l| l.0);
    let mut had = Vec::new();
    for &c in copies {
        had.push(ed.get(c, keys[0])?.is_some());
    }
    let shrink = |ed: &mut Editor, t: u64| -> Result<(), Error> {
        if ed.get(t, keys[2])?.is_none_or(|i| i.len() < 24) {
            return Err(Error::Format(format!("inode item of directory {dirid} missing")));
        }
        ed.update(t, keys[2], |d| {
            let size = u64_at(d, 16).saturating_sub(2 * name.len() as u64);
            d[16..24].copy_from_slice(&size.to_le_bytes());
        })
    };
    let unlink_in = |ed: &mut Editor, t: u64| -> Result<bool, Error> {
        let mut found = false;
        if ed.get(t, keys[0])?.is_some() {
            ed.delete(t, keys[0])?;
            found = true;
        }
        if let Some(rest) = ed.get(t, keys[1])?.map(|d| without_entry(&d, name)).transpose()?.flatten() {
            if rest.is_empty() {
                ed.delete(t, keys[1])?;
            } else {
                ed.replace(t, keys[1], &rest)?;
            }
            found = true;
        }
        Ok(found)
    };
    if !unlink_in(ed, tree)? {
        return Err(Error::Format("directory entry not found".into()));
    }
    shrink(ed, tree)?;
    for (i, &c) in copies.iter().enumerate() {
        // Entries in nodes shared with `tree` are gone already; the copy's own nodes still have them.
        unlink_in(ed, c)?;
        let shared_inode = leaf(&paths[i + 1][2]) == leaf(&paths[0][2]);
        match (had[i], shared_inode) {
            (true, false) => shrink(ed, c)?,
            (false, true) => return Err(Error::Unsupported("directory size shared with a snapshot without the entry".into())),
            _ => {}
        }
    }
    for p in all {
        ed.repair_keys(p)?;
    }
    Ok(())
}

const INODE_REF_KEY: u8 = 12;
const FIRST_FREE: u64 = 256;
const LAST_FREE: u64 = u64::MAX - 256;

/// Subvolumes of an unmounted filesystem (a restored disk), with their paths from the top level,
/// read from the trees alone: ROOT_REF items and the INODE_REF chain of the directory holding
/// each one.
pub fn offline_subvolumes(fs: &Filesystem) -> Result<Vec<Subvolume>, Error> {
    let mut refs = Vec::new();
    fs.walk(fs.sb.root, 0, &mut |_, key, data| {
        if key.kind == ROOT_REF_KEY && (FIRST_FREE..=LAST_FREE).contains(&key.offset) {
            let (dirid, _, name) = root_ref(data)?;
            refs.push((key.objectid, key.offset, dirid, String::from_utf8_lossy(name).into_owned()));
        }
        Ok(())
    })?;
    let items = fs.root_items()?;
    let tree_of = |id: u64| items.iter().find(|(i, _)| *i == id).map(|(_, d)| u64_at(d, 176));
    let ed = Editor::new(fs);
    let mut done: Vec<Subvolume> = Vec::new();
    let mut pending = refs;
    while !pending.is_empty() {
        let before = pending.len();
        let mut rest = Vec::new();
        for (parent, id, dirid, name) in pending {
            let parent_path = if parent == FS_TREE_OBJECTID {
                Some(PathBuf::new())
            } else {
                done.iter().find(|s| s.id == parent).map(|s| s.path.clone())
            };
            let Some(pp) = parent_path else {
                rest.push((parent, id, dirid, name));
                continue;
            };
            let tree = tree_of(parent).ok_or_else(|| Error::Format(format!("tree of subvolume {parent} not found")))?;
            // Directory path inside the parent, walking up to its root directory (256).
            let mut parts = Vec::new();
            let mut ino = dirid;
            while ino != FIRST_FREE_OBJECTID {
                let (key, data) = ed
                    .find_first(tree, ino, INODE_REF_KEY)?
                    .ok_or_else(|| Error::Format(format!("inode {ino} of subvolume {parent} has no name")))?;
                let len = data.get(8..10).map_or(0, |l| u16::from_le_bytes([l[0], l[1]]) as usize);
                let name = data.get(10..10 + len).ok_or_else(|| Error::Format(format!("inode ref of {ino} truncated")))?;
                parts.push(String::from_utf8_lossy(name).into_owned());
                ino = key.offset;
            }
            let mut path = pp;
            for p in parts.iter().rev() {
                path.push(p);
            }
            done.push(Subvolume { id, parent, path: path.join(name) });
        }
        if rest.len() == before {
            return Err(Error::Format("subvolumes with parent not found".into()));
        }
        pending = rest;
    }
    done.sort_by_key(|s| s.id);
    Ok(done)
}

/// Restore-time exclusion: the subvolumes matching `patterns`, those nested in them and their
/// snapshots (by parent UUID, e.g. btrbk's and the agent's own), are deleted in the image as
/// [`plan`] does at backup time. Returns the plan and the subvolumes matched directly (to be
/// recreated empty), or None if nothing matches.
pub fn restore_plan(fs: &Filesystem, patterns: &[String]) -> Result<Option<(Exclusion, Vec<PathBuf>)>, Error> {
    let subvols = offline_subvolumes(fs)?;
    let direct: Vec<&Subvolume> = subvols
        .iter()
        .filter(|s| patterns.iter().any(|p| glob(p.trim_matches('/'), &s.path.to_string_lossy())))
        .collect();
    if direct.is_empty() {
        return Ok(None);
    }
    let items = fs.root_items()?;
    // Root items written by kernels since 3.x carry uuid (247) and parent_uuid (263).
    let uuids = |id: u64| {
        items
            .iter()
            .find(|(i, _)| *i == id)
            .filter(|(_, d)| d.len() >= 279)
            .map(|(_, d)| (d[247..263].to_vec(), d[263..279].to_vec()))
    };
    let mut set: HashSet<u64> = direct.iter().map(|s| s.id).collect();
    loop {
        let before = set.len();
        let own: Vec<Vec<u8>> = set.iter().filter_map(|&id| uuids(id)).map(|(u, _)| u).collect();
        for s in &subvols {
            let snapshot_of_set = uuids(s.id).is_some_and(|(_, parent)| parent != [0; 16] && own.contains(&parent));
            if set.contains(&s.parent) || snapshot_of_set {
                set.insert(s.id);
            }
        }
        if set.len() == before {
            break;
        }
    }
    // The agent's snapshots in the image (`<SNAP_DIR>/<run>/<source id>.<name>`) share their
    // source's nodes: they are edited along with it.
    let (pinned, snapshots): (Vec<Pinned>, Vec<u64>) = subvols
        .iter()
        .filter(|s| s.path.starts_with(crate::prepare::SNAP_DIR) && s.path.components().count() == 3)
        .filter_map(|s| {
            let src = s.path.file_name()?.to_string_lossy().split('.').next()?.parse().ok()?;
            Some((Pinned { subvol: src, snapshot_transid: 0 }, s.id))
        })
        .unzip();
    let map = crate::usedmap::UsedMap::build(fs)?;
    let data = map.ranges.iter().filter(|r| r.kind == crate::usedmap::Kind::Data).map(|r| (r.logical, r.len));
    let picked = direct.iter().map(|s| s.path.clone()).collect();
    Ok(plan_ids(fs, &subvols, &pinned, &snapshots, set, data)?.map(|e| (e, picked)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globbing() {
        assert!(glob("@games", "@games"));
        assert!(!glob("@games", "@games2"));
        assert!(glob("btrbk_snapshots/@games.*", "btrbk_snapshots/@games.20261020T1200"));
        assert!(glob("*cache*", "@home/user/.cache"));
        assert!(!glob("@home/?", "@home/ab"));
    }

    #[test]
    fn shared_nodes_found_from_their_refs() {
        // A tree and the agent's snapshot of it: roots r and s, both pointing to node a, which
        // points to leaf l.
        let tree: edit::Path = vec![(10, 0), (20, 1), (30, 2)];
        let snap: edit::Path = vec![(11, 0), (20, 1), (30, 2)];
        let refs = |extra: &'static [(u64, u64)]| {
            move |n: u64| Ok(match n {
                20 => 2,
                _ => 1,
            } + extra.iter().filter(|e| e.0 == n).map(|e| e.1).sum::<u64>())
        };
        assert_eq!(shared_node(&[&tree, &snap], &mut refs(&[])).unwrap(), None);
        // A third tree (snapper) points to a, or to l through a node of its own.
        assert_eq!(shared_node(&[&tree, &snap], &mut refs(&[(20, 1)])).unwrap(), Some(20));
        assert_eq!(shared_node(&[&tree, &snap], &mut refs(&[(30, 1)])).unwrap(), Some(30));
        // Without the snapshot's path, a's second reference is unexplained.
        assert_eq!(shared_node(&[&tree], &mut refs(&[])).unwrap(), Some(20));
    }

    #[test]
    fn restore_exclusion_by_id_and_consistent() {
        use std::os::unix::fs::FileExt;
        let dir = std::env::temp_dir().join(format!("btrfs-image-test-{}-exclude", std::process::id()));
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("keep")).unwrap();
        for i in 0..1500 {
            std::fs::write(root.join("keep").join(format!("file-with-a-long-name-{i}")), i.to_string()).unwrap();
        }
        for d in ["g*", "games"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
            std::fs::write(root.join(d).join("f"), d).unwrap();
        }
        let img = dir.join("fs.img");
        std::fs::File::create(&img).unwrap().set_len(256 << 20).unwrap();
        let ok = std::process::Command::new("mkfs.btrfs")
            .args(["-q", "-f", "--rootdir"])
            .arg(&root)
            .args(["--subvol", "g*", "--subvol", "games"])
            .arg(&img)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("mkfs.btrfs not available: skipped");
            return;
        }
        let fs = Filesystem::open(&img).unwrap();
        // "g?" matches "g*" only; its path used as a pattern would match "games" too.
        let (ex, picked) = restore_plan(&fs, &["g?".into()]).unwrap().unwrap();
        assert_eq!(picked, vec![PathBuf::from("g*")]);
        assert_eq!(ex.subvolumes, vec![PathBuf::from("g*")]);
        let f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        for (logical, node) in &ex.nodes {
            for phys in fs.physical(*logical).unwrap() {
                f.write_all_at(node, phys).unwrap();
            }
        }
        drop(f);
        let check = std::process::Command::new("btrfs").args(["check", "--readonly"]).arg(&img).output();
        if let Ok(out) = check {
            assert!(out.status.success(), "btrfs check: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash() {
        // Known value: btrfs name hash of "default" (the default subvolume entry).
        assert_eq!(name_hash(b"default"), 0x8dbf_c2d2);
    }
}
