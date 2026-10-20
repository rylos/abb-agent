#!/bin/bash
# Test of ext-image on ext2/3/4 with various options: used blocks must match dumpe2fs, and an
# image with only the used blocks must pass e2fsck -fn and contain the same files.
# Usage: sudo tests/ext.sh <work dir>
set -euo pipefail
WORK=$(realpath -m "${1:?work dir}")
BIN=$(dirname "$(realpath "$0")")/../target/release/ext-usedmap
mkdir -p "$WORK/m" "$WORK/i"
LOOP=""
cleanup() {
    mountpoint -q "$WORK/m" && umount "$WORK/m" || true
    mountpoint -q "$WORK/i" && umount "$WORK/i" || true
    [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true
}
trap cleanup EXIT
run() { # run <name> <mkfs options>
    local name=$1; shift
    rm -f "$WORK/fs.img" "$WORK/out.img"
    truncate -s 1500M "$WORK/fs.img"
    LOOP=$(losetup -f --show "$WORK/fs.img")
    "$@" -q -F "$LOOP" >/dev/null
    mount "$LOOP" "$WORK/m"
    for i in $(seq 1 40); do head -c $(( (RANDOM % 8192 + 1) * 1024 )) /dev/urandom > "$WORK/m/f$i"; done
    mkdir -p "$WORK/m/d/e"; for i in $(seq 1 300); do echo "$i" > "$WORK/m/d/e/s$i"; done
    rm -f "$WORK/m"/f1[0-9]
    umount "$WORK/m"
    local free total
    free=$(dumpe2fs -h "$LOOP" 2>/dev/null | awk -F: '/^Free blocks/ {gsub(/ /,"",$2); print $2}')
    total=$(dumpe2fs -h "$LOOP" 2>/dev/null | awk -F: '/^Block count/ {gsub(/ /,"",$2); print $2}')
    local out
    out=$("$BIN" "$LOOP" --image "$WORK/out.img")
    local used=$(echo "$out" | sed -n 's/.* \([0-9]*\) used .*/\1/p')
    local fsck=OK
    e2fsck -fn "$WORK/out.img" >/dev/null 2>&1 || fsck=ERROR
    mount -o ro "$LOOP" "$WORK/m"; mount -o ro,loop "$WORK/out.img" "$WORK/i"
    local diff=same
    (cd "$WORK/m" && find . -type f -exec sha256sum {} + | sort) > "$WORK/a"
    (cd "$WORK/i" && find . -type f -exec sha256sum {} + | sort) > "$WORK/b"
    cmp -s "$WORK/a" "$WORK/b" || diff=DIFFERENT
    umount "$WORK/m" "$WORK/i"
    losetup -d "$LOOP"; LOOP=""
    printf '%-22s dumpe2fs used %-8s ext-image %-8s e2fsck %-7s file %s\n' "$name" $((total - free)) "$used" "$fsck" "$diff"
}
run ext2 mkfs.ext2
run ext3 mkfs.ext3
run ext4 mkfs.ext4
run ext4-1k mkfs.ext4 -b 1024 -O ^orphan_file  # orphan_file with 1 KiB blocks: kernel 7.2 will not mount it
run ext4-meta_bg mkfs.ext4 -O meta_bg,^resize_inode
run ext4-no64 mkfs.ext4 -O ^64bit,^metadata_csum
run ext4-noflex mkfs.ext4 -O ^flex_bg
run ext4-sparse2 mkfs.ext4 -O sparse_super2
run ext3-1k mkfs.ext3 -b 1024
run ext2-nosparse mkfs.ext2 -O ^sparse_super,^resize_inode
