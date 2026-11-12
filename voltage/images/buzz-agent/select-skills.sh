#!/bin/sh
# Prepare the `engineering-skills` build context for the agent image.
#
#   select-skills.sh <engineering-skills checkout> <output directory>
#   select-skills.sh --none <output directory>
#
# voltagecloud/engineering-skills is internal and this repository is public,
# so the skills never enter this repository or the main build context. CI
# checks out the repository's master branch on main and passes the result to
# `docker build --build-context engineering-skills=<output directory>`. Pull
# requests and local builds pass an empty context made with --none.
#
# The output holds skills/<name>/ for each shipped skill and REVISION, the
# commit the skills come from ("none" for an empty context). This script
# prints counts only: the CI logs are public.
set -eu

# Skills that need tools the image does not have (VHS).
EXCLUDE="cli-demo-video"

usage() {
    echo "usage: $0 <engineering-skills checkout> <output directory>" >&2
    echo "       $0 --none <output directory>" >&2
    exit 2
}

[ "$#" -eq 2 ] || usage
out=$2
rm -rf "$out"
mkdir -p "$out/skills"

if [ "$1" = --none ]; then
    echo none > "$out/REVISION"
    echo "engineering skills: none (empty build context)"
    exit 0
fi

source=$1
[ -d "$source/skills" ] || { echo "no skills/ directory in the checkout" >&2; exit 1; }
shipped=0
skipped=0
for dir in "$source"/skills/*/; do
    [ -f "$dir/SKILL.md" ] || continue
    name=$(basename "$dir")
    case " $EXCLUDE " in
        *" $name "*) skipped=$((skipped + 1)); continue ;;
    esac
    cp -R "$dir" "$out/skills/$name"
    shipped=$((shipped + 1))
done
[ "$shipped" -gt 0 ] || { echo "the checkout has no shippable skills" >&2; exit 1; }
git -C "$source" rev-parse HEAD > "$out/REVISION"
echo "engineering skills: $shipped shipped, $skipped excluded, revision $(cat "$out/REVISION")"
