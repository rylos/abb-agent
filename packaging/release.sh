#!/bin/sh
# Builds the release files in dist/: the Arch Linux package and the static abb-restore binary,
# from the committed tree (HEAD), with their SHA-256 sums.
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

# abb-restore for live systems: static, no dependencies.
cargo build --release --target x86_64-unknown-linux-musl -p abb-restore
cp target/x86_64-unknown-linux-musl/release/abb-restore "$dist/abb-restore-$version-x86_64"
strip "$dist/abb-restore-$version-x86_64"

rm -rf "$dist/build"
(cd "$dist" && sha256sum -- * >SHA256SUMS)
ls -l "$dist"
