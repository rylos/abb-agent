//! Read-only diagnostics: map of the used blocks of a btrfs.
//! Usage: sudo btrfs-usedmap /dev/nvme0n1p2

use btrfs_image::Filesystem;
use btrfs_image::usedmap::{Kind, UsedMap};
use std::time::Instant;

const GIB: f64 = (1u64 << 30) as f64;

fn main() {
    let Some(dev) = std::env::args().nth(1) else {
        eprintln!("usage: btrfs-usedmap <device>");
        std::process::exit(2);
    };
    let t = Instant::now();
    let fs = Filesystem::open(dev.as_ref()).unwrap_or_else(|e| fail(e));
    let sb = &fs.sb;
    println!(
        "generation {}  nodesize {}  device {:.1} GiB  bytes_used {:.2} GiB  chunk {}",
        sb.generation,
        sb.nodesize,
        sb.total_bytes as f64 / GIB,
        sb.bytes_used as f64 / GIB,
        fs.chunks.len()
    );
    if sb.log_root != 0 {
        println!("tree-log present (log_root {}): it will be discarded in the image", sb.log_root);
    }

    let map = UsedMap::build(&fs).unwrap_or_else(|e| fail(e));
    let s = &map.stats;
    println!(
        "nodes {} ({:.2} GiB logical)  data extents {} ({:.2} GiB)  physical bytes {:.2} GiB",
        s.tree_blocks,
        s.tree_bytes_logical as f64 / GIB,
        s.data_extents,
        s.data_bytes as f64 / GIB,
        s.physical_bytes as f64 / GIB
    );
    println!(
        "superblock bytes_used {:.2} GiB, extent sum {:.2} GiB",
        sb.bytes_used as f64 / GIB,
        (s.data_bytes + s.tree_bytes_logical) as f64 / GIB
    );

    if std::env::args().any(|a| a == "--cbt") {
        let t3 = Instant::now();
        let cap = btrfs_image::capture::Capture::default();
        let d = btrfs_image::cbt::Digests::compute(&fs, &cap, &map).unwrap_or_else(|e| fail(e));
        let used = d.table.iter().filter(|v| **v != 0).count();
        println!("fingerprints: {} blocks of 64 KiB, {} used, in {:.1}s", d.table.len(), used, t3.elapsed().as_secs_f64());
        return;
    }
    let block = 1u64 << 20;
    let bits = map.bitmap(sb.total_bytes, block);
    let used = bits.iter().filter(|b| **b).count();
    println!(
        "1 MiB blocks to send: {used} of {} ({:.2} GiB)",
        bits.len(),
        used as f64 * block as f64 / GIB
    );

    // Spot check: re-reads all nodes (header + checksum) as in a real send.
    let t2 = Instant::now();
    let mut bad = 0;
    let mut checked = 0;
    let mut by_owner: std::collections::BTreeMap<u64, u64> = Default::default();
    let mut seen = std::collections::HashSet::new();
    for r in map.ranges.iter().filter(|r| r.kind == Kind::Tree) {
        checked += 1;
        let res = fs.read_node(r.logical, 0);
        if let Ok(n) = &res
            && seen.insert(r.logical) {
                *by_owner.entry(n.header.owner).or_default() += 1;
            }
        if let Err(e) = res {
            bad += 1;
            if bad <= 5 {
                println!("  {e}");
            }
        }
    }
    let mut owners: Vec<_> = by_owner.into_iter().collect();
    owners.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (owner, n) in owners.iter().take(12) {
        println!("  tree {:>20}  nodes {:>8}  {:>8.1} MiB", *owner as i64, n, *n as f64 * sb.nodesize as f64 / 1048576.0);
    }
    println!("nodes re-read {checked}, changed {bad}, in {:.1}s", t2.elapsed().as_secs_f64());
    println!("total time {:.1}s", t.elapsed().as_secs_f64());
}

fn fail(e: btrfs_image::Error) -> ! {
    eprintln!("error: {e}");
    std::process::exit(1);
}
