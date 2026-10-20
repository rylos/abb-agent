# abb-agent

<h3 align="center">
An Active Backup for Business agent that runs on any Linux distribution and any kernel, 7.x
included: written in Rust, no kernel module to build, nothing to break on the next update.<br>
The way it should have been from the start.
</h3>

A Linux agent for **Synology Active Backup for Business** (ABB) that makes full-disk image
backups of machines whose root filesystem is **btrfs**, and also of ext2/3/4 volumes. It talks to an
unmodified ABB server: the device shows up in the ABB console like any other physical
server, backups are versioned, deduplicated and restorable with Synology's tools.

Written in **Rust**: one self-contained binary, memory-safe by design. It needs
no kernel module and no Synology binaries.

## Supported filesystems

> ### 🟢 btrfs: first-class support
> The filesystem the official agent cannot back up. abb-agent reads btrfs natively: read-only
> snapshots of every subvolume, only the blocks in use, every data block verified against the
> btrfs checksums, incremental backups from btrfs metadata, subvolumes that can be left out.

| filesystem | backup | incremental | notes |
|---|---|---|---|
| **btrfs** | ✅ | ✅ | single device, any subvolume layout (`@`, `@home`, snapper, btrbk…) |
| **ext4** | ✅ | ✅ | read-write mounted volumes are skipped, or frozen if you allow it |
| **ext3**, **ext2** | ✅ | ✅ | as ext4 |
| vfat (EFI system partition), BIOS boot | ✅ | ✅ | copied as they are |
| XFS, LVM, LUKS, md-raid, multi-device btrfs | ❌ | | left out, the backup is reported as partial |

Disks must use GPT partition tables.

> **Status: experimental.** Backups and incremental backups work and have been verified
> by checking the images stored on the server (`btrfs check`, scrub, `e2fsck`, file
> comparison) and by booting them; bare-metal restores with `abb-restore` and with Synology's
> recovery media boot. Do not use it as your only backup yet.

This project is not affiliated with, endorsed or supported by Synology Inc. "Synology" and
"Active Backup for Business" are trademarks of Synology Inc.

## Why

The official Linux agent relies on a kernel module (`synosnap`) that tracks block writes.
On btrfs that approach does not work, and the module has to be rebuilt for every kernel.
abb-agent takes a different route: btrfs already knows which blocks are in use and
keeps checksums of them, so a consistent image can be produced from the raw device with
read-only snapshots and a bit of care.

## Features

- Whole-disk images: GPT, EFI system partition, BIOS boot partition and every volume, so a
  restored disk has the same partitions, UUIDs and boot loader.
- **btrfs**: read-only snapshots of all subvolumes, then only the blocks in use at that
  transaction are sent. Every data block is verified against the btrfs checksum tree before it
  leaves the machine. Snapshots are removed when the backup ends.
- **Incremental backups** (changed-block tracking) computed from btrfs metadata, without
  reading the data again. In-place writes such as NOCOW files and preallocated extents are
  detected too.
- **Subvolume exclusion**: btrfs subvolumes (e.g. a games library or a package cache) can be
  left out of the image. In the image they become deleted subvolumes that btrfs removes on the
  first mount, so the restored filesystem stays consistent; their data is not sent. A small boot
  unit recreates them empty after a restore, so mounts in `/etc/fstab` keep working.
- **ext2/3/4**: used blocks from the block-group bitmaps. Unmounted and read-only volumes are
  imaged directly; read-write mounted ones are skipped or, if you allow it, frozen during the
  backup.
- Schedule decided by the agent: after every boot and every N hours, with retries. Backups
  can also be started from the ABB console.
- Parallel read, verification and LZ4 compression (several GB/s on NVMe).
- Mutual exclusion with other backup jobs through lock files.

See [Supported filesystems](#supported-filesystems) for what is left out.

## Requirements

- Linux with btrfs and/or ext4, GPT disks, x86_64 or aarch64.
- A Synology NAS with Active Backup for Business (tested with ABB 3.2.1 on DSM 7.4), and a DSM
  account allowed to use ABB.
- Root privileges on the client: the agent reads block devices and creates snapshots.

## Install

Arch Linux:

```sh
git clone https://github.com/rylos/abb-agent.git
cd abb-agent/packaging/arch
makepkg -si
```

From source on any distribution (Rust 1.88 or newer):

```sh
cargo build --release -p abb-agent
sudo install -Dm755 target/release/abb-agent /usr/bin/abb-agent
sudo install -Dm644 packaging/systemd/abb-agent.service /usr/lib/systemd/system/abb-agent.service
sudo install -Dm644 packaging/systemd/abb-agent-fixup.service /usr/lib/systemd/system/abb-agent-fixup.service
sudo install -Dm644 packaging/abb-agent.toml /etc/abb-agent.toml
```

The restore tool is built as a static binary, to copy onto a USB stick next to any live image:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl -p abb-restore
# target/x86_64-unknown-linux-musl/release/abb-restore
```

## Usage

1. Connect the machine to the server. This creates the device and its backup task in ABB, using
   the server's default template:

   ```sh
   sudo abb-agent connect -a nas.example.lan -u backupuser
   ```

   The password is asked interactively, or read from `ABB_PASSWORD`. The server
   certificate is pinned on first contact.

2. Review `/etc/abb-agent.toml` (all settings are optional; see the comments in the file).

3. Start the service:

   ```sh
   sudo systemctl enable --now abb-agent
   sudo systemctl enable abb-agent-fixup   # only needed if you exclude subvolumes
   ```

Other commands:

| command | |
|---|---|
| `abb-agent status` | connection, tasks, last backup and next scheduled one |
| `abb-agent backup` | run a backup now, in the foreground |
| `abb-agent config` | show the configuration in effect |
| `abb-agent disks [disk]` | disks, partitions and filesystems as the agent sees them |
| `abb-agent bench [disk] [--limit GiB]` | everything a full backup does except sending, to measure speed |

### What the ABB console controls

The task settings in the ABB console apply as they do with the official agent:

| console setting | behaviour |
|---|---|
| Source type | *Entire device*: every internal disk. *System volume*: the volumes holding `/` and `/boot` plus the EFI/BIOS boot partitions. *Custom volume*: the volumes picked in the console (btrfs volumes are offered too). Partition tables are always kept, so the disks can be restored bare-metal with all their partitions; volumes left out are restored empty. |
| Schedule and backup window | followed in server time; a run missed while the machine was off happens when it comes back; a backup still running when the window closes is cancelled |
| Bandwidth limit | applied to the upload, also when changed during a backup |
| Pre/post scripts | the pre script runs before the snapshots, the post script right after them, as root with `/bin/sh` (180 s limit); output and result are reported to the server |
| Back up now / Cancel | start and stop a backup at any time |
| Retention, permissions, backup verification | handled by the server |

On top of that, `/etc/abb-agent.toml` can add a backup after every boot (on by default), a fixed
interval, retries after a failure and lock files to wait for.

## Restore

The server stores one image per disk. Files can be browsed and restored from the ABB portal.

### Bare-metal restore with abb-restore

`abb-restore` is a single static binary (no dependencies) that restores a whole machine from any
Linux live system: Arch ISO, SystemRescue, Ubuntu live, anything with a network and a shell.

```sh
# on the live system, as root
./abb-restore
```

A text interface walks through the same steps as Synology's recovery wizard: NAS address and
account, device and task, restore mode, version, disk mapping, summary and progress.

| mode | what it does |
|---|---|
| Entire device | every disk of the backup onto a whole local disk, partition table included |
| System volumes | only the EFI/BIOS boot partitions and the volumes holding `/` and `/boot`, in place |
| Selected volumes | chosen volumes onto existing partitions or new partitions in free space |

- Only the areas in use are transferred, and the restore shows up in the ABB console like one made
  with Synology's media.
- The target disk can be **bigger** than the original: the backup GPT is moved to the end of the
  disk and, if you want, the last partition and its btrfs or ext filesystem are grown to fill it.
  A **smaller** disk works as long as the partitions fit on it.
- Partition UUIDs and filesystem UUIDs are kept, so `/etc/fstab` and boot entries keep working.
- On UEFI machines the boot loader (systemd-boot, GRUB or shim) is copied to `\EFI\BOOT\` when
  missing, so the restored disk boots on a new machine without an NVRAM entry.

For scripts the same restore runs without the interface:

```sh
abb-restore list     -a nas.example.lan -u backupuser
abb-restore versions -a nas.example.lan -u backupuser -t 2
abb-restore run      -a nas.example.lan -u backupuser -t 2 --device --map sda=nvme0n1 --grow --yes
```

Synology's own recovery media works too: the agent reports the device so that it is listed there.

### After a restore

The agent removes, on its first start, the temporary snapshots that were part of the image.
Excluded subvolumes are gone in the restored system; `abb-agent-fixup` recreates them empty early
at boot (it reads the list from `/var/lib/abb-agent`, which must be on the root filesystem). Your
own btrfs snapshots (btrbk, snapper, …) are restored with everything else. The first backup after a
restore is a full one.

## How it works

1. **Snapshot.** For each btrfs volume the agent creates a read-only snapshot of every subvolume
   under `/.abb-snapshots/<run>` on the top-level subvolume. It then reads the superblock of the
   transaction that committed them.
2. **Capture.** Blocks that the snapshots do not pin, such as the global trees, the subvolume
   roots and what was written between snapshots, are read and verified right away and kept in
   memory.
3. **Map.** The extent tree of that transaction gives the exact set of blocks in use. Everything
   else is sent as unused.
4. **Stream.** The disk is sent in order. Used blocks are read with `O_DIRECT`, patched from the
   capture, verified (tree nodes by their header checksum, data by the checksum tree) and
   compressed. Superblocks are rewritten without the tree log.
5. **Incrementals.** Each 64 KiB block has a 64-bit fingerprint, derived from the data checksums
   and from tree node addresses and generations. Blocks whose fingerprint did not change since the
   previous version are not sent.

ext volumes follow the same scheme, using the block bitmaps instead of the extent tree. Their
fingerprints are computed while reading.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Bug reports with `abb-agent disks` output and the
journal of the service (`journalctl -u abb-agent`) are very welcome.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
