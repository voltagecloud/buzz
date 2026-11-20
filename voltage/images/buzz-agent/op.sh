#!/bin/sh
set -eu
# The profile's service account token stays in a file, out of process environments,
# so bare `op` would report no accounts. Load it unless the caller set one
# (profile.py passes its own while it bootstraps, before the file exists).
if [ -z "${OP_SERVICE_ACCOUNT_TOKEN:-}" ] && [ -f "$HOME/.config/buzz-agent/op-token" ]; then
    OP_SERVICE_ACCOUNT_TOKEN=$(cat "$HOME/.config/buzz-agent/op-token")
    export OP_SERVICE_ACCOUNT_TOKEN
    unset OP_CONNECT_HOST OP_CONNECT_TOKEN OP_SESSION
fi
exec /opt/agent-tools/op "$@"
