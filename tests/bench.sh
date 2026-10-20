#!/bin/bash
# Test of `abb-agent bench` on a scratch GPT disk (loop device): ESP, btrfs with subvolumes,
# compressed and NOCOW files, ext4 mounted read-write, ext3 unmounted, swap; ongoing writes
# to btrfs and ext4 (ext4 is excluded, or frozen with `freeze`). With `corrupt` it alters a data sector on disk
# (underneath btrfs) and expects the checksum verification to report it.
# Usage: sudo tests/bench.sh <work dir> [corrupt|freeze|swap]
set -euo pipefail

WORK=$(realpath -m "${1:?work dir}")
MODE=${2:-}
BIN=$(dirname "$(realpath "$0")")/../target/release/abb-agent
mkdir -p "$WORK/top" "$WORK/ext"
LOOP=""; WRITER=""; SWAP=""
cleanup() {
    [ -n "$SWAP" ] && swapoff "$SWAP" || true
    if [ -n "$WRITER" ]; then
        kill -STOP "$WRITER"; pkill -P "$WRITER"; kill -9 "$WRITER"; wait "$WRITER" 2>/dev/null || true
        sleep 0.5
    fi
    # Writers blocked by a freeze can take a moment to go away.
    for m in "$WORK/top" "$WORK/ext"; do
        for _ in 1 2 3 4 5 6 7 8 9 10; do mountpoint -q "$m" || break; umount "$m" 2>/dev/null || sleep 0.5; done
    done
    [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true
}
trap cleanup EXIT

rm -f "$WORK/disk.img"
truncate -s 4G "$WORK/disk.img"
printf "label: gpt\nsize=64M, type=U\nsize=2G, type=L\nsize=600M, type=L\nsize=300M, type=L\nsize=64M, type=S\n" | sfdisk -q "$WORK/disk.img"
LOOP=$(losetup -f --show -P "$WORK/disk.img")
mkfs.vfat -F32 "${LOOP}p1" >/dev/null
mkfs.btrfs -q -f -m dup -d single "${LOOP}p2"
mount -o compress=zstd:3 "${LOOP}p2" "$WORK/top"
mkfs.ext4 -q -F "${LOOP}p3"; mount "${LOOP}p3" "$WORK/ext"
mkfs.ext3 -q -F "${LOOP}p4"; mkswap -q "${LOOP}p5"
for i in $(seq 1 30); do head -c $(( (RANDOM % 8192 + 64) * 1024 )) /dev/urandom > "$WORK/ext/e$i"; done
btrfs -q subvolume create "$WORK/top/@"
btrfs -q subvolume create "$WORK/top/@home"
for i in $(seq 1 60); do head -c $(( (RANDOM % 4096 + 64) * 1024 )) /dev/urandom > "$WORK/top/@/r$i"; done
for i in $(seq 1 20); do yes "compressible text $i" 2>/dev/null | head -c 4M > "$WORK/top/@home/t$i" || true; done
mkdir "$WORK/top/@home/nocow"; chattr +C "$WORK/top/@home/nocow"
head -c 32M /dev/urandom > "$WORK/top/@home/nocow/vm.img"
fallocate -l 16M "$WORK/top/@home/nocow/prealloc"
sync

if [ "$MODE" = swap ]; then
    btrfs -q subvolume create "$WORK/top/@swap"
    btrfs filesystem mkswapfile -s 64M "$WORK/top/@swap/swapfile" >/dev/null
    swapon "$WORK/top/@swap/swapfile"
    SWAP="$WORK/top/@swap/swapfile"
fi
if [ "$MODE" = corrupt ]; then
    # First extent of r1: btrfs logical address → physical offset in the partition.
    LOGICAL=$(filefrag -v -b1 "$WORK/top/@/r1" | awk '/^ *0:/ {gsub(/\.\./," "); print $4; exit}')
    PHYS=$(btrfs-map-logical -l "$LOGICAL" "${LOOP}p2" 2>/dev/null | awk '/physical/ {print $6; exit}')
    echo "r1: logical $LOGICAL physical $PHYS: corrupting 512 bytes on disk"
    sync; echo 3 > /proc/sys/vm/drop_caches
    dd if=/dev/urandom of="${LOOP}p2" bs=512 count=1 seek=$(( PHYS / 512 + 3 )) oflag=direct conv=notrunc status=none
else
    ( n=0; while true; do n=$((n+1)); head -c 1M /dev/urandom > "$WORK/top/@home/w$((n % 50))"; \
      head -c 256K /dev/urandom > "$WORK/ext/w$((n % 20))"; \
      dd if=/dev/urandom of="$WORK/top/@home/nocow/vm.img" bs=64K count=4 seek=$((RANDOM % 500)) conv=notrunc status=none; done ) &
    WRITER=$!
fi
"$BIN" bench "$(basename "$LOOP")" ${MODE:+$([ "$MODE" = freeze ] && echo --freeze-ext)}
