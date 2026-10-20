//! Read-only diagnostics: used blocks of an ext2/3/4 and, with `--image <file>`, a copy of
//! only the used blocks to a file (the rest zeroed) to check it with e2fsck.
//! Usage: sudo ext-usedmap /dev/sdXN [--image out.img]

use btrfs_image::device::Device;
use ext_image::ExtFs;
use std::os::unix::fs::FileExt;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(dev) = args.get(1) else {
        eprintln!("usage: ext-usedmap <device> [--image <file>]");
        std::process::exit(2);
    };
    let fs = ExtFs::open(dev.as_ref()).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1)
    });
    println!(
        "block {} B, {} groups, {} blocks, {} used ({} ranges)",
        fs.block_size,
        fs.groups,
        fs.size / fs.block_size,
        fs.used_bytes() / fs.block_size,
        fs.ranges.len()
    );
    if args.iter().any(|a| a == "--inodes") {
        let t = std::time::Instant::now();
        let i = fs.inodes().unwrap_or_else(|e| {
            eprintln!("error: {e}");
            std::process::exit(1)
        });
        let owned: u64 = i.owned.iter().map(|o| o.1).sum();
        // Owned blocks must all be in use.
        let outside: u64 = i.owned.iter().filter(|o| !fs.any_used(o.0, o.0 + o.1)).map(|o| o.1).sum();
        println!(
            "inodes in use {}, owned {:.1} MiB in {} ranges ({} B outside the used blocks), free inode table blocks {}, in {:.2}s",
            i.sigs.len(),
            owned as f64 / 1048576.0,
            i.owned.len(),
            outside,
            i.free_itable.len(),
            t.elapsed().as_secs_f64()
        );
        let meta = fs.metadata_ranges(&i);
        let file: u64 = i.file_data.iter().map(|r| r.1).sum();
        let journal: u64 = i.journal.iter().map(|r| r.1).sum();
        println!(
            "file data {:.1} MiB, journal {:.1} MiB, metadata to capture {:.1} MiB in {} ranges",
            file as f64 / 1048576.0,
            journal as f64 / 1048576.0,
            meta.iter().map(|r| r.1).sum::<u64>() as f64 / 1048576.0,
            meta.len()
        );
    }
    if let Some(i) = args.iter().position(|a| a == "--image") {
        let out = std::fs::File::create(&args[i + 1]).expect("image file");
        out.set_len(fs.size).unwrap();
        let d = Device::open(dev.as_ref()).unwrap();
        let mut buf = vec![0u8; 1 << 20];
        let mut w = 0;
        while w < fs.size {
            let n = (1u64 << 20).min(fs.size - w) as usize;
            if fs.any_used(w, w + n as u64) {
                d.read_exact_at(&mut buf[..n], w).unwrap();
                fs.fill_window(w, &mut buf[..n]);
                out.write_all_at(&buf[..n], w).unwrap();
            }
            w += n as u64;
        }
        println!("image written");
    }
}
