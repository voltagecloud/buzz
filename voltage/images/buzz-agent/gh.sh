#!/bin/sh
set -eu
if [ -f "$HOME/.config/buzz-agent/github-token" ]; then
    GH_TOKEN=$(cat "$HOME/.config/buzz-agent/github-token")
    export GH_TOKEN
fi
exec /usr/bin/gh "$@"
