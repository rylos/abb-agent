//! Consistency test: snapshots all subvolumes, reads the generation, writes an image of the
//! used blocks to a file. Meant for a test filesystem.
//!
//! Usage: sudo btrfs-imagetest <device> <mounted subvolid=5 root> <image.img> [pause_ms]
//! `pause_ms` slows down the copy (pause every 8 one-MiB windows) to simulate a long backup while
//! the filesystem is being modified.

use btrfs_image::usedmap::UsedMap;
use btrfs_image::capture::{self, Pinned};
use btrfs_image::disk::u64_at;
use btrfs_image::{Filesystem, image, snapshot};
use std::path::Path;
use std::time::{Duration, Instant};

const SNAP_DIR: &str = ".abb-snapshots";

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: btrfs-imagetest <device> <subvolid=5 root> <image> [pause_ms]");
        std::process::exit(2);
    }
    let (dev, top, out) = (Path::new(&a[1]), Path::new(&a[2]), Path::new(&a[3]));
    let pause = Duration::from_millis(a.get(4).map_or(0, |p| p.parse().unwrap()));
    let t = Instant::now();

    // 1. Read-only snapshots of all subvolumes (excluding ours).
    let fs = Filesystem::open(dev).unwrap_or_else(|e| fail(e));
    let snap_root = top.join(SNAP_DIR);
    std::fs::create_dir_all(&snap_root).unwrap();
    let run = format!("run-{}", std::process::id());
    let run_dir = snap_root.join(&run);
    std::fs::create_dir(&run_dir).unwrap();
    let subvols = snapshot::list_subvolumes(&fs, top).unwrap_or_else(|e| fail(e));
    let mut n = 0;
    for s in subvols.iter().filter(|s| !s.path.starts_with(SNAP_DIR)) {
        let name = format!("{}.{}", s.id, s.path.to_string_lossy().replace('/', "_"));
        snapshot::create_readonly(&top.join(&s.path), &run_dir, &name).unwrap_or_else(|e| fail(e));
        n += 1;
    }
    println!("snapshots created: {n} in {:.2}s", t.elapsed().as_secs_f64());

    // 2. Superblock of the generation just committed and map of used blocks.
    let mut fs = Filesystem::open(dev).unwrap_or_else(|e| fail(e));
    println!("generation {}", fs.sb.generation);

    // Snapshots of this run (named "<source id>.<path>") and their creation transaction.
    let after = snapshot::list_subvolumes(&fs, top).unwrap_or_else(|e| fail(e));
    let run_prefix = Path::new(SNAP_DIR).join(&run);
    let items = fs.root_items().unwrap_or_else(|e| fail(e));
    let otransid = |id: u64| items.iter().find(|(i, _)| *i == id).map(|(_, d)| u64_at(d, 303)).unwrap();
    let mut pinned = Vec::new();
    let mut ours = Vec::new();
    for s in &after {
        if let Ok(rest) = s.path.strip_prefix(&run_prefix) {
            let src: u64 = rest.to_string_lossy().split('.').next().unwrap().parse().unwrap();
            pinned.push(Pinned { subvol: src, snapshot_transid: otransid(s.id) });
            ours.push(s.id);
        } else if s.path.starts_with(SNAP_DIR) {
            // Snapshots from previous runs: immutable, managed by the agent.
            ours.push(s.id);
        }
    }
    let tc = Instant::now();
    let cap = capture::capture(&fs, &pinned, &ours, &[]).unwrap_or_else(|e| fail(e));
    println!(
        "capture: {} nodes ({:.1} MiB), {} data extents ({:.1} MiB) in {:.2}s",
        cap.nodes.len(),
        cap.node_bytes() as f64 / 1048576.0,
        cap.data.len(),
        cap.data_bytes as f64 / 1048576.0,
        tc.elapsed().as_secs_f64()
    );
    fs.use_capture(cap.nodes.clone());
    let map = UsedMap::build(&fs).unwrap_or_else(|e| fail(e));
    println!(
        "ranges {}  bytes {:.1} MiB",
        map.ranges.len(),
        map.stats.physical_bytes as f64 / 1048576.0
    );

    // 3. Copy (optionally slowed down) to file.
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(out).unwrap();
    file.set_len(fs.sb.total_bytes).unwrap();
    let mut written = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    let mut i = 0u64;
    let mut start = 0u64;
    while start < fs.sb.total_bytes {
        let len = (1u64 << 20).min(fs.sb.total_bytes - start) as usize;
        let w = &mut buf[..len];
        fs.read_at(w, start).unwrap_or_else(|e| fail(e));
        if image::fill_window(&fs, &cap, &map, start, w).unwrap_or_else(|e| fail(e)) {
            std::os::unix::fs::FileExt::write_all_at(&file, w, start).unwrap();
            written += len as u64;
            i += 1;
            if !pause.is_zero() && i.is_multiple_of(8) {
                std::thread::sleep(pause);
            }
        }
        start += len as u64;
    }
    println!("written {:.1} MiB in {:.1}s", written as f64 / 1048576.0, t.elapsed().as_secs_f64());
    println!("snapshots in {}", run_dir.display());
}

fn fail(e: btrfs_image::Error) -> ! {
    eprintln!("error: {e}");
    std::process::exit(1);
}
