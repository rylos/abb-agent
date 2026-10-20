#!/bin/sh
# Signs a published release with the release key: the git tag (verified on GitHub) and every
# file of the GitHub release (a detached .asc next to each). gpg asks for the passphrase once.
#   packaging/sign-release.sh <version>
set -eu
version=$1
tag=v$version
key=B903BF2CD82C9EA45C9FB35DE2E8944F09676A61
repo=rylos/abb-agent
cd "$(dirname "$0")/.."

git fetch -q --tags --force origin
commit=$(git rev-parse "$tag^{commit}")
git -c user.signingkey=$key tag -s -f "$tag" "$commit" -m "abb-agent $version"
git tag -v "$tag"
git push -f origin "$tag"

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
gh release download "$tag" --repo "$repo" --dir "$dir" --pattern '*'
rm -f "$dir"/*.asc
for f in "$dir"/*; do
  gpg --local-user "$key" --armor --detach-sign --yes "$f"
  gpg --verify "$f.asc" "$f"
done
gh release upload "$tag" --repo "$repo" --clobber "$dir"/*.asc
echo "signed $tag and its files"
