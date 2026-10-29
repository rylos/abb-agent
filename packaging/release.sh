#!/bin/sh
# Builds the release files in dist/ from the committed tree (HEAD), with their SHA-256 sums: the
# Arch Linux package, the binaries with the units and the configuration (used by abb-agent-bin)
# and the static abb-restore binary. Everything comes from one export of HEAD: the working tree
# must be clean, and nothing from it is used.
#   packaging/release.sh
set -eu
cd "$(dirname "$0")/.."
if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
  echo "release.sh: the working tree has changes: commit or stash them first" >&2
  exit 1
fi
commit=$(git rev-parse HEAD)
version=$(git show "$commit:crates/abb-agent/Cargo.toml" | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)
# Reproducible archives and packages: times from the commit, not from the build.
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct "$commit")
export SOURCE_DATE_EPOCH
# The binaries go to other computers: built for any x86-64 CPU. Flags from the environment
# (or a -C target-cpu=native in it) would leak into them: the 0.2.x binaries built with the
# host's -march died with SIGILL on older CPUs (an i5-7300U).
unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS CARGO_TARGET_DIR
dist=$(pwd)/dist
rm -rf "$dist" && mkdir -p "$dist/build"
tgz() { # tgz <archive> <directory> <entry>: owner, order and times fixed
  tar -C "$2" --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner -cf - "$3" | gzip -9n >"$1"
}

# The source, as GitHub serves it for the tag (git archive is reproducible), and a copy of it
# to build the static binary from.
git archive --format=tar.gz --prefix="abb-agent-$version/" "$commit" >"$dist/build/abb-agent-$version.tar.gz"
mkdir -p "$dist/build/src"
tar -xzf "$dist/build/abb-agent-$version.tar.gz" -C "$dist/build/src"
src=$dist/build/src/abb-agent-$version

# Arch Linux package.
cp "$src/packaging/arch/abb-agent/PKGBUILD" "$src/packaging/arch/abb-agent/abb-agent.install" "$dist/build/"
sum=$(sha256sum "$dist/build/abb-agent-$version.tar.gz" | cut -d' ' -f1)
sed -i "s/^pkgver=.*/pkgver=$version/; s/^sha256sums=.*/sha256sums=('$sum')/" "$dist/build/PKGBUILD"
cp /etc/makepkg.conf "$dist/build/makepkg.conf"
cat >>"$dist/build/makepkg.conf" <<'CONF'
CFLAGS="-march=x86-64 -mtune=generic -O2 -pipe -fno-plt -fexceptions -Wp,-D_FORTIFY_SOURCE=3 -Wformat -Werror=format-security -fstack-clash-protection -fcf-protection"
CXXFLAGS="$CFLAGS -Wp,-D_GLIBCXX_ASSERTIONS"
RUSTFLAGS="-C opt-level=3 -C target-cpu=x86-64"
CONF
# --config also leaves out the host's makepkg.conf.d (rust.conf sets target-cpu=native) and
# ~/.makepkg.conf; -C: nothing reused from an earlier build.
(cd "$dist/build" && makepkg -fC --noconfirm --config ./makepkg.conf)
mv "$dist"/build/abb-agent-"$version"-*.pkg.tar.zst "$dist/"

# The same binaries with the units and the configuration, for abb-agent-bin.
bin="abb-agent-$version-x86_64"
mkdir -p "$dist/build/$bin" "$dist/build/pkg"
tar -xf "$dist"/abb-agent-"$version"-*-x86_64.pkg.tar.zst -C "$dist/build/pkg"
cp "$dist/build/pkg/usr/bin/abb-agent" "$dist/build/pkg/usr/bin/abb-restore" \
  "$src/packaging/systemd/abb-agent.service" "$src/packaging/systemd/abb-agent-fixup.service" \
  "$src/packaging/systemd/abb-agent.tmpfiles" "$src/packaging/abb-agent.toml" "$src/README.md" "$src/LICENSE" \
  "$dist/build/$bin/"
tgz "$dist/$bin.tar.gz" "$dist/build" "$bin"

# abb-restore for live systems: static, no dependencies.
(cd "$src" && RUSTFLAGS="-C target-cpu=x86-64" CARGO_TARGET_DIR="$dist/build/target" \
  cargo build --locked --release --target x86_64-unknown-linux-musl -p abb-restore)
cp "$dist/build/target/x86_64-unknown-linux-musl/release/abb-restore" "$dist/abb-restore-$version-x86_64"
strip "$dist/abb-restore-$version-x86_64"

rm -rf "$dist/build"
(cd "$dist" && sha256sum -- * >SHA256SUMS)
echo "release $version from $commit"
ls -l "$dist"
