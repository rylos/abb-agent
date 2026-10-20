#!/bin/bash
# Consistency test of btrfs-imagetest on a scratch btrfs in a loop device.
# Usage: sudo tests/coherence.sh <work dir> [pause_ms]
# Result: btrfs check on the image, scrub (checksums of all blocks) and sha256 comparison
# of the snapshot contents between the original filesystem and the image.
set -euo pipefail

WORK=${1:?work dir}
PAUSE=${2:-20}
BIN=$(dirname "$0")/../target/release/btrfs-imagetest
mkdir -p "$WORK"
WORK=$(realpath "$WORK")
TOP=$WORK/top
MNT=$WORK/img
mkdir -p "$TOP" "$MNT"

LOOP=""; LOOP2=""; WRITER=""
cleanup() {
    [ -n "$WRITER" ] && kill "$WRITER" 2>/dev/null && wait "$WRITER" 2>/dev/null || true
    mountpoint -q "$TOP" && umount "$TOP" || true
    mountpoint -q "$MNT" && umount "$MNT" || true
    [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true
    [ -n "$LOOP2" ] && losetup -d "$LOOP2" 2>/dev/null || true
}
trap cleanup EXIT

truncate -s 4G "$WORK/fs.img"
LOOP=$(losetup -f --show "$WORK/fs.img")
mkfs.btrfs -q -f -m dup -d single "$LOOP"
mount -o compress=zstd:3 "$LOOP" "$TOP"

btrfs -q subvolume create "$TOP/@"
btrfs -q subvolume create "$TOP/@home"
mkdir -p "$TOP/@home/user"
btrfs -q subvolume create "$TOP/@home/user/Games"
mkdir -p "$TOP/btrbk_snapshots"

fill() { # fill <dir> <n files>
    for i in $(seq 1 "$2"); do
        head -c $(( (RANDOM % 2048 + 64) * 1024 )) /dev/urandom > "$1/f$i"
    done
    # compressible file
    yes "line of text $1" | head -c 20M > "$1/text.txt" || true
}
fill "$TOP/@" 150
fill "$TOP/@home/user" 120
fill "$TOP/@home/user/Games" 60
btrfs -q subvolume snapshot -r "$TOP/@" "$TOP/btrbk_snapshots/@.20261020T1200"
sync

# Writer: modifies, deletes and creates files until stopped; fsync exercises the tree-log.
writer() {
    local dirs=("$TOP/@" "$TOP/@home/user" "$TOP/@home/user/Games")
    local n=0
    while true; do
        local d=${dirs[$((RANDOM % 3))]}
        local f="$d/f$((RANDOM % 200 + 1))"
        case $((RANDOM % 4)) in
            0) rm -f "$f" ;;
            1) head -c $(( (RANDOM % 1024 + 16) * 1024 )) /dev/urandom > "$f" ;;
            2) [ -f "$f" ] && dd if=/dev/urandom of="$f" bs=4k count=8 seek=$((RANDOM % 16)) conv=notrunc,fsync status=none ;;
            3) head -c 65536 /dev/urandom > "$d/new$n"; sync -d "$d/new$n"; n=$((n + 1)) ;;
        esac
    done
}
writer &
WRITER=$!
sleep 2

"$BIN" "$LOOP" "$TOP" "$WORK/out.img" "$PAUSE" | tee "$WORK/imagetest.log"
RUN_DIR=$(sed -n 's/^snapshots in //p' "$WORK/imagetest.log")

kill "$WRITER"; wait "$WRITER" 2>/dev/null || true; WRITER=""
sync

manifest() { # manifest <root> <file>
    (cd "$1" && for s in .abb-snapshots/*/*; do
        (cd "$s" && find . -type f -print0 | sort -z | xargs -0 -r sha256sum | sed "s|^|$s |")
    done) > "$2"
}
manifest "$TOP" "$WORK/live.sha"
umount "$TOP"; losetup -d "$LOOP"; LOOP=""

echo "== btrfs check"
btrfs check --readonly "$WORK/out.img" > "$WORK/check.log" 2>&1 && echo "check OK (exit 0)" || { echo "check FAILED"; grep -iE "error|mismatch|wrong|corrupt" "$WORK/check.log" | head; }

LOOP2=$(losetup -f --show -r "$WORK/out.img")
mount -o ro "$LOOP2" "$MNT"
echo "== scrub"
btrfs scrub start -B -r "$MNT" | grep -E "Error summary|Total to scrub|Status"
manifest "$MNT" "$WORK/image.sha"
echo "== snapshot comparison: $(wc -l < "$WORK/live.sha") files"
if diff -q "$WORK/live.sha" "$WORK/image.sha" >/dev/null; then
    echo "OK: identical content"
else
    echo "DIFFERENCES:"; diff "$WORK/live.sha" "$WORK/image.sha" | head -20
    exit 1
fi
