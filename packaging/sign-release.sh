#!/bin/sh
# Signs a published release with the release key: the git tag (verified on GitHub) and every
# file of the GitHub release (a detached .asc next to each). gpg asks for the passphrase once.
# Only files identical to the ones release.sh built here (dist/SHA256SUMS) are signed.
#   packaging/sign-release.sh <version>
set -eu
version=$1
tag=v$version
key=B903BF2CD82C9EA45C9FB35DE2E8944F09676A61
repo=rylos/abb-agent
cd "$(dirname "$0")/.."
sums=$(pwd)/dist/SHA256SUMS
if ! grep -q -- "-$version-" "$sums" 2>/dev/null; then
  echo "sign-release.sh: no $sums of release $version: run it where packaging/release.sh built it" >&2
  exit 1
fi

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
gh release download "$tag" --repo "$repo" --dir "$dir" --pattern '*'
rm -f "$dir"/*.asc
# What GitHub serves must be what was built here: every file listed, with the same content.
(cd "$dir" && sha256sum --strict --quiet -c "$sums") || {
  echo "sign-release.sh: the release files differ from $sums: nothing signed" >&2
  exit 1
}
for f in "$dir"/*; do
  n=${f##*/}
  if [ "$n" = SHA256SUMS ]; then
    cmp -s "$f" "$sums" || { echo "sign-release.sh: SHA256SUMS of the release differs from $sums" >&2; exit 1; }
  elif ! cut -d' ' -f3- "$sums" | grep -qxF -- "$n"; then
    echo "sign-release.sh: $n is in the release but not in $sums" >&2
    exit 1
  fi
done

git fetch -q --tags --force origin
commit=$(git rev-parse "$tag^{commit}")
# The tagger must be an address GitHub verifies for the account (the old-style noreply one).
git -c user.signingkey=$key -c user.email=rylos@users.noreply.github.com tag -s -f "$tag" "$commit" -m "abb-agent $version"
git tag -v "$tag"
git push -f origin "$tag"

for f in "$dir"/*; do
  gpg --local-user "$key" --armor --detach-sign --yes "$f"
  gpg --verify "$f.asc" "$f"
done
gh release upload "$tag" --repo "$repo" --clobber "$dir"/*.asc
echo "signed $tag and its files"
