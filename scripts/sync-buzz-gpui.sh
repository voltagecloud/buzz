#!/usr/bin/env bash
# Bring upstream benthecarman/buzz-gpui changes into apps/buzz-gpui.
#
# usage: scripts/sync-buzz-gpui.sh [ref]   (default: master)
#
# Applies only the upstream diff between the commit recorded in
# apps/buzz-gpui/UPSTREAM and <ref>, as a three-way merge, so local changes
# under apps/buzz-gpui survive. On a conflict the script stops with conflict
# markers in the working tree; resolve them, then rerun with --record <ref> to
# update UPSTREAM. The result is left staged for a signed, signed-off commit.
set -euo pipefail

prefix=apps/buzz-gpui
upstream_file="$prefix/UPSTREAM"
cd "$(git rev-parse --show-toplevel)"

value() { sed -n "s/^$1=//p" "$upstream_file"; }
repository=$(value repository)
old=$(value commit)

record=false
if [ "${1:-}" = "--record" ]; then record=true; shift; fi
ref=${1:-master}

git fetch --quiet --depth 1 "$repository" "$old"
git fetch --quiet --depth 1 "$repository" "$ref"
new=$(git rev-parse FETCH_HEAD^{commit})

write_upstream() {
  sed -i "s|^ref=.*|ref=$ref|; s|^commit=.*|commit=$new|" "$upstream_file"
  git add "$upstream_file"
  echo "Recorded $repository $ref ($new) in $upstream_file."
}

if $record; then write_upstream; exit 0; fi
if [ "$old" = "$new" ]; then echo "Already at $ref ($new)."; exit 0; fi
if [ -n "$(git status --porcelain -- "$prefix")" ]; then
  echo "Commit or stash changes under $prefix first." >&2; exit 1
fi

patch=$(mktemp)
trap 'rm -f "$patch"' EXIT
git diff --binary --full-index "$old" "$new" > "$patch"
if [ ! -s "$patch" ]; then write_upstream; exit 0; fi

if git apply --3way --index --directory="$prefix" "$patch"; then
  write_upstream
  echo "Applied $repository ${old:0:12}..${new:0:12}. Review, then commit with -S -s."
else
  echo "Conflicts under $prefix. Resolve them, git add the files, then run:" >&2
  echo "  scripts/sync-buzz-gpui.sh --record $ref" >&2
  exit 1
fi
