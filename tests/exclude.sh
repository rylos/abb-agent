#!/bin/bash
# Test of subvolume exclusion: a GPT test disk with a btrfs holding @, @home, @games (with a
# nested subvolume and a btrbk-style snapshot), under write load. The image written by
# `abb-agent bench --image` with @games and its snapshots excluded must pass `btrfs check`,
# mount, drop the excluded subvolumes in the cleaner, pass check and scrub again, and keep the
# other files intact; data used only by @games must not be in the image.
# Usage: sudo tests/exclude.sh <work dir>
set -euo pipefail
WORK=$(realpath -m "${1:?work dir}")
BIN=$(dirname "$(realpath "$0")")/../target/release/abb-agent
mkdir -p "$WORK/top" "$WORK/img"
LOOP=""; LOOP2=""; WRITER=""
cleanup() {
    if [ -n "$WRITER" ]; then kill -STOP "$WRITER"; pkill -P "$WRITER"; kill -9 "$WRITER"; wait "$WRITER" 2>/dev/null || true; sleep 0.5; fi
    for m in "$WORK/top" "$WORK/img"; do
        for _ in 1 2 3 4 5 6 7 8 9 10; do mountpoint -q "$m" || break; umount "$m" 2>/dev/null || sleep 0.5; done
    done
    [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true
    [ -n "$LOOP2" ] && losetup -d "$LOOP2" 2>/dev/null || true
}
trap cleanup EXIT

rm -f "$WORK/disk.img" "$WORK"/out.*
truncate -s 3G "$WORK/disk.img"
printf "label: gpt\nsize=64M, type=U\ntype=L\n" | sfdisk -q "$WORK/disk.img"
LOOP=$(losetup -f --show -P "$WORK/disk.img")
mkfs.vfat -F32 "${LOOP}p1" >/dev/null
mkfs.btrfs -q -f -m dup -d single "${LOOP}p2"
mount "${LOOP}p2" "$WORK/top"
T=$WORK/top
btrfs -q subvolume create "$T/@"
btrfs -q subvolume create "$T/@home"
btrfs -q subvolume create "$T/@games"
btrfs -q subvolume create "$T/@games/shadercache"
mkdir -p "$T/btrbk_snapshots"
for i in $(seq 1 40); do head -c $(( (RANDOM % 2048 + 64) * 1024 )) /dev/urandom > "$T/@/f$i"; done
for i in $(seq 1 30); do head -c $(( (RANDOM % 2048 + 64) * 1024 )) /dev/urandom > "$T/@home/h$i"; done
for i in $(seq 1 12); do head -c 32M /dev/urandom > "$T/@games/game$i.pak"; done
head -c 16M /dev/urandom > "$T/@games/shadercache/cache.bin"
btrfs -q subvolume snapshot -r "$T/@games" "$T/btrbk_snapshots/@games.20261020T1200"
# Data written after the snapshot: only in @games.
for i in $(seq 13 16); do head -c 32M /dev/urandom > "$T/@games/game$i.pak"; done
cp --reflink=always "$T/@games/game1.pak" "$T/@/reflinked-from-games.pak"
sync

( n=0; while true; do n=$((n+1)); head -c 1M /dev/urandom > "$T/@home/w$((n % 30))"; \
  head -c 1M /dev/urandom > "$T/@games/w$((n % 30))"; done ) &
WRITER=$!

printf '[btrfs]\nexclude_subvolumes = ["@games", "btrbk_snapshots/@games.*"]\n' > "$WORK/config.toml"
ABB_AGENT_CONFIG="$WORK/config.toml" "$BIN" bench "$(basename "$LOOP")" --image "$WORK/out" 2>&1 | grep -v "^  "

kill -STOP "$WRITER"; pkill -P "$WRITER"; kill -9 "$WRITER"; wait "$WRITER" 2>/dev/null || true; WRITER=""
sync
(cd "$T/@" && sha256sum f* reflinked-from-games.pak | sort) > "$WORK/sum-root"
(cd "$T/@home" && sha256sum h* | sort) > "$WORK/sum-home"
umount "$T"

IMG=$(ls "$WORK"/out.*.img)
echo "image: $(du -h --apparent-size "$IMG" | cut -f1) apparent, $(du -h "$IMG" | cut -f1) on disk"
LOOP2=$(losetup -f --show -P "$IMG")
echo "== btrfs check (before mount)"
btrfs check --readonly "${LOOP2}p2" 2>&1 | grep -iE "error|found|orphan|dead|ok" | head -8 || true
mount "${LOOP2}p2" "$WORK/img"
echo "== subvolumes after mount"
btrfs subvolume list "$WORK/img" | grep -v abb-snapshots || true
btrfs subvolume sync "$WORK/img"
# The cleaner frees the extents in the next transactions.
for _ in 1 2 3; do btrfs filesystem sync "$WORK/img"; sleep 1; done
echo "== after the cleaner"
btrfs subvolume list "$WORK/img" | grep -v abb-snapshots || true
ls "$WORK/img"
(cd "$WORK/img/@" && sha256sum f* reflinked-from-games.pak | sort) | cmp -s - "$WORK/sum-root" && echo "@: files identical" || echo "@: FILES DIFFER"
(cd "$WORK/img/@home" && sha256sum h* | sort) | cmp -s - "$WORK/sum-home" && echo "@home: files identical" || echo "@home: FILES DIFFER"
btrfs scrub start -B "$WORK/img" 2>&1 | grep -E "Total to scrub|Error summary" || true
btrfs filesystem df "$WORK/img" | grep -i data
umount "$WORK/img"
echo "== btrfs check (after the cleaner)"
btrfs check --readonly "${LOOP2}p2" 2>&1 | grep -iE "error|found" | head -5 || true
