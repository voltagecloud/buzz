#!/bin/sh
set -eu
if [ -f "$HOME/.config/buzz-agent/github-token" ]; then
    GH_TOKEN=$(cat "$HOME/.config/buzz-agent/github-token")
    export GH_TOKEN
fi
# gh finds extensions only under its data directory, so link the one the image carries.
extensions="${XDG_DATA_HOME:-$HOME/.local/share}/gh/extensions"
if [ ! -e "$extensions/gh-stack" ] && [ -d /opt/gh-extensions/gh-stack ]; then
    mkdir -p "$extensions" 2>/dev/null && ln -s /opt/gh-extensions/gh-stack "$extensions/gh-stack" 2>/dev/null || true
fi
exec /usr/bin/gh "$@"
