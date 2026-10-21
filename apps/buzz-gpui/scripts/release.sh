#!/usr/bin/env bash
# Prepares a release: sets the workspace version, refreshes Cargo.lock,
# commits the bump and makes an annotated tag. It pushes nothing; pushing
# the tag starts .github/workflows/build.yml, which builds the packages
# and publishes the GitHub release that installed copies update from.
#
# Usage: scripts/release.sh <version>          e.g. 0.1.1 or 0.2.0-rc.1
#        scripts/release.sh --bump-only <version>
#                                              only edit the files
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"

bump_only=false
if [ "${1:-}" = "--bump-only" ]; then
  bump_only=true
  shift
fi
version="${1:-}"
version="${version#v}"
semver='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$'
if ! [[ "$version" =~ $semver ]]; then
  sed -n '7,9p' "$0" >&2
  exit 2
fi
tag="v$version"

current="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)"
if [ -z "$current" ]; then
  echo "No version under [workspace.package] in Cargo.toml" >&2
  exit 1
fi
if [ "$current" = "$version" ]; then
  echo "The workspace is already at $version" >&2
  exit 1
fi
if ! $bump_only; then
  if [ -n "$(git status --porcelain)" ]; then
    echo "Commit or stash your changes first" >&2
    exit 1
  fi
  if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    echo "Tag $tag already exists" >&2
    exit 1
  fi
fi

# Every crate takes its version from [workspace.package].
sed -i.bak "/^\[workspace.package\]/,/^\[/s/^version = \".*\"/version = \"$version\"/" Cargo.toml
rm -f Cargo.toml.bak
# Rewrites only the workspace members' entries in the lock file.
cargo update --workspace --offline --quiet
cargo metadata --locked --offline --format-version 1 >/dev/null

if $bump_only; then
  echo "Set the version to $version (not committed)"
  exit 0
fi

git commit --quiet -m "Release $tag" -- Cargo.toml Cargo.lock
git tag -a "$tag" -m "Buzz GPUI $version"
branch="$(git rev-parse --abbrev-ref HEAD)"
cat <<EOF
Committed the bump and tagged $tag ($current -> $version). Nothing was pushed.

To publish, push the commit and then the tag:

  git push origin $branch
  git push origin $tag

The Build workflow then builds the packages, writes SHA256SUMS and
publishes the release. To undo before pushing:

  git tag -d $tag && git reset --hard HEAD~1
EOF
