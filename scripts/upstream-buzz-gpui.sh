#!/usr/bin/env bash
# Offer Voltage changes under apps/buzz-gpui back to benthecarman/buzz-gpui.
#
# usage: scripts/upstream-buzz-gpui.sh [--dry-run] [--open] [--base <ref>] \
#          <fork> <branch> <commit-range>
#
#   <fork>          push target: an owner/repo on GitHub (for example
#                   voltagecloud/buzz-gpui) or a Git URL
#   <branch>        branch to create on the fork
#   <commit-range>  buzz commits to offer, for example HEAD~2..HEAD or a single
#                   <sha> (treated as <sha>^..<sha>)
#   --base <ref>    upstream ref to build on (default: master)
#   --dry-run       build and verify the branch locally; do not push
#   --open          after pushing, open a draft PR against upstream
#
# The commits must change only apps/buzz-gpui, and never apps/buzz-gpui/UPSTREAM.
# Paths are rewritten relative to apps/buzz-gpui, applied with `git am -3` onto
# the upstream base, and signed. Keep Voltage-only wiring out of these commits.
set -euo pipefail

prefix=apps/buzz-gpui
dry_run=false open=false base=master
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry_run=true; shift ;;
    --open) open=true; shift ;;
    --base) base=$2; shift 2 ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
[ $# -eq 3 ] || { sed -n '2,20p' "$0" >&2; exit 2; }
fork=$1 branch=$2 range=$3
case "$range" in *..*) ;; *) range="$range^..$range" ;; esac

cd "$(git rev-parse --show-toplevel)"
repository=$(sed -n 's/^repository=//p' "$prefix/UPSTREAM")
upstream_slug=$(sed -E 's#^https://github.com/([^/]+/[^/.]+)(\.git)?$#\1#' <<<"$repository")

[ -n "$(git rev-list "$range")" ] || { echo "No commits in $range." >&2; exit 1; }
# format-patch leaves merge commits out, and with them any conflict
# resolution they carry.
if [ -n "$(git rev-list --merges "$range")" ]; then
  echo "$range contains merge commits. Offer a linear series of commits." >&2
  exit 1
fi
# Check each commit, not just the range's endpoints: format-patch --relative
# silently drops changes outside $prefix, even ones a later commit reverts.
refused=false
for commit in $(git rev-list --reverse "$range"); do
  files=$(git diff-tree --no-commit-id --name-only -r "$commit")
  outside=$(grep -v "^$prefix/" <<<"$files" || true)
  if [ -n "$outside" ]; then
    echo "$(git log -1 --format='%h %s' "$commit") also changes files outside $prefix:" >&2
    sed 's/^/  /' <<<"$outside" >&2
    refused=true
  fi
  if grep -qx "$prefix/UPSTREAM" <<<"$files"; then
    echo "$(git log -1 --format='%h %s' "$commit") changes $prefix/UPSTREAM, which is Voltage-only." >&2
    refused=true
  fi
done
if $refused; then
  echo "Split the upstreamable part into its own commits." >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git format-patch --quiet --relative="$prefix" -o "$work/patches" "$range"

git clone --quiet --depth 200 --branch "$base" "$repository" "$work/upstream"
# A tag base leaves no origin/<base> ref, so list the new commits from this.
base_commit=$(git -C "$work/upstream" rev-parse HEAD)
git -C "$work/upstream" checkout --quiet -b "$branch"
if ! git -C "$work/upstream" am --quiet -3 -S "$work"/patches/*.patch; then
  git -C "$work/upstream" am --abort || true
  echo "The commits do not apply onto $upstream_slug $base. Sync first, or rebase them." >&2
  exit 1
fi
echo "Built $branch on $upstream_slug $base:"
git -C "$work/upstream" log --format='  %h %G? %s' "$base_commit..HEAD"

if $dry_run; then echo "Dry run: nothing pushed."; exit 0; fi

case "$fork" in
  *://*|git@*) push_url=$fork ;;
  *) push_url="git@github.com:$fork.git" ;;
esac
git -C "$work/upstream" push --quiet "$push_url" "$branch"
fork_owner=$(sed -E 's#^(ssh://)?(git@github\.com[:/]|https://github\.com/)?([^/]+)/.*#\3#' <<<"$fork")
echo "Pushed $branch to $fork."
if $open; then
  gh pr create --repo "$upstream_slug" --base "$base" --head "$fork_owner:$branch" --fill --draft
else
  echo "Open the PR with:"
  echo "  gh pr create --repo $upstream_slug --base $base --head $fork_owner:$branch --fill"
fi
