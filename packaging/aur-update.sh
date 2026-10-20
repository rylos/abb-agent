#!/bin/sh
# Updates the AUR packages abb-agent and abb-agent-bin to a published GitHub release: version,
# checksums of the release files and .SRCINFO, in local clones of the AUR repositories.
#   packaging/aur-update.sh <version> <directory of the AUR clones>
# Then review, commit and push each clone (git -C <dir>/abb-agent push).
set -eu
version=$1
aur=$2
here=$(cd "$(dirname "$0")" && pwd)
url=https://github.com/rylos/abb-agent
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for pkg in abb-agent abb-agent-bin; do
  [ -d "$aur/$pkg/.git" ] || git clone "ssh://aur@aur.archlinux.org/$pkg.git" "$aur/$pkg"
  cp "$here/arch/$pkg/PKGBUILD" "$here/arch/$pkg/abb-agent.install" "$aur/$pkg/"
done

curl -fsSL -o "$tmp/src.tar.gz" "$url/archive/v$version.tar.gz"
curl -fsSL -o "$tmp/bin.tar.gz" "$url/releases/download/v$version/abb-agent-$version-x86_64.tar.gz"
src=$(sha256sum "$tmp/src.tar.gz" | cut -d' ' -f1)
bin=$(sha256sum "$tmp/bin.tar.gz" | cut -d' ' -f1)
sed -i "s/^pkgver=.*/pkgver=$version/; s/^pkgrel=.*/pkgrel=1/; s/^sha256sums=.*/sha256sums=('$src')/" "$aur/abb-agent/PKGBUILD"
sed -i "s/^pkgver=.*/pkgver=$version/; s/^pkgrel=.*/pkgrel=1/; s/^sha256sums_x86_64=.*/sha256sums_x86_64=('$bin' 'SKIP')/" "$aur/abb-agent-bin/PKGBUILD"
for pkg in abb-agent abb-agent-bin; do
  (cd "$aur/$pkg" && makepkg --printsrcinfo >.SRCINFO && git add PKGBUILD abb-agent.install .SRCINFO && git status --short)
done
