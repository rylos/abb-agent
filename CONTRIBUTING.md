# Contributing

Thanks for helping. A few notes to make it smooth.

## Layout

| crate | role |
|---|---|
| `crates/abb-proto` | wire format and transport for the ABB agent protocol (TLS with certificate pinning) |
| `crates/btrfs-image` | read-only btrfs parser on the raw device: chunk map, tree walks with verification, used-block map, snapshots, capture, change fingerprints |
| `crates/ext-image` | read-only ext2/3/4 used-block map |
| `crates/abb-agent` | the agent: CLI, service, schedule, device description, upload stream |

## Building and testing

```sh
cargo build --release
cargo clippy --release
```

The scripts in `tests/` need root and create loop devices in a working directory you pass as an
argument:

- `tests/bench.sh <dir> [corrupt|freeze|swap]` builds a GPT test disk (ESP, btrfs, ext4, ext3,
  swap) under write load and runs `abb-agent bench` on it. With `corrupt`, the run must fail on the
  corrupted block.
- `tests/ext.sh <dir>` checks the ext used-block map against `dumpe2fs` and `e2fsck` for ten
  filesystem variants.
- `tests/coherence.sh <dir>` writes an image of a btrfs under write load and checks it with
  `btrfs check`, scrub and file hashes.

Please run the relevant ones before sending a pull request that touches the image code. Never run
experiments against a production ABB server: connecting a device creates a device and a task on
it.

## Guidelines

- Reading the disk must never write to it, apart from the snapshots the agent creates and
  removes.
- Anything sent to the server must be verified or provably consistent. When in doubt, fail the
  backup rather than store a wrong image.
- Keep dependencies few and pure Rust.
- Commit messages and code comments in English.

## Protocol changes

The agent identifies itself as the official agent version it was written against. If a new ABB
server release rejects it or behaves differently, please open an issue with the server version
(DSM and ABB package) and the agent's journal.
