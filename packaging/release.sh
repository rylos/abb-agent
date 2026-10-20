#!/bin/sh
# Builds the release files in dist/ from the committed tree (HEAD), with their SHA-256 sums: the
# Arch Linux package, the binaries with the units and the configuration (used by abb-agent-bin)
# and the static abb-restore binary.
#   packaging/release.sh
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/abb-agent/Cargo.toml | head -1)
dist=dist
rm -rf "$dist" && mkdir -p "$dist/build"

# Arch Linux package, from a source archive identical to the one GitHub serves for the tag.
git archive --format=tar.gz --prefix="abb-agent-$version/" HEAD >"$dist/build/abb-agent-$version.tar.gz"
cp packaging/arch/abb-agent/PKGBUILD packaging/arch/abb-agent/abb-agent.install "$dist/build/"
sum=$(sha256sum "$dist/build/abb-agent-$version.tar.gz" | cut -d' ' -f1)
sed -i "s/^pkgver=.*/pkgver=$version/; s/^sha256sums=.*/sha256sums=('$sum')/" "$dist/build/PKGBUILD"
(cd "$dist/build" && makepkg -f --noconfirm)
mv "$dist"/build/abb-agent-"$version"-*.pkg.tar.zst "$dist/"

# The same binaries with the units and the configuration, for abb-agent-bin.
bin="abb-agent-$version-x86_64"
mkdir -p "$dist/build/$bin" "$dist/build/pkg"
tar -xf "$dist"/abb-agent-"$version"-*-x86_64.pkg.tar.zst -C "$dist/build/pkg"
cp "$dist/build/pkg/usr/bin/abb-agent" "$dist/build/pkg/usr/bin/abb-restore" \
  packaging/systemd/abb-agent.service packaging/systemd/abb-agent-fixup.service \
  packaging/abb-agent.toml README.md LICENSE "$dist/build/$bin/"
tar -C "$dist/build" --owner=0 --group=0 -czf "$dist/$bin.tar.gz" "$bin"

# abb-restore for live systems: static, no dependencies.
cargo build --release --target x86_64-unknown-linux-musl -p abb-restore
cp target/x86_64-unknown-linux-musl/release/abb-restore "$dist/abb-restore-$version-x86_64"
strip "$dist/abb-restore-$version-x86_64"

rm -rf "$dist/build"
(cd "$dist" && sha256sum -- * >SHA256SUMS)
ls -l "$dist"
