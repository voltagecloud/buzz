#!/bin/sh
set -eu
# The daemon gets its own enrolled profile, never the Sandbox profile or
# op-token file. An absent profile fails in via without falling back to the
# user's default config. This wrapper selects a profile; it is not an
# authorization boundary. The enrolled provider must enforce read authority.
exec /opt/agent-tools/via --config "$HOME/.config/paseo/via.toml" "$@"
