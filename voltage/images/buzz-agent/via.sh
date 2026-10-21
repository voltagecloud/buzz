#!/bin/sh
set -eu
# Session code is trusted with this dedicated development vault's grants.
if [ -f "$HOME/.config/buzz-agent/op-token" ]; then
    OP_SERVICE_ACCOUNT_TOKEN=$(cat "$HOME/.config/buzz-agent/op-token")
    export OP_SERVICE_ACCOUNT_TOKEN
    unset OP_CONNECT_HOST OP_CONNECT_TOKEN OP_SESSION
fi
exec /opt/agent-tools/via --config "$HOME/.config/buzz-agent/via.toml" "$@"
