#!/bin/bash
set -euo pipefail

# A recreated Sandbox Pod has a new emptyDir. Its original credentials cannot
# silently launch a new consumer with an empty workspace.
if [[ -n "${BUZZ_SANDBOX_POD_UID+x}" || -n "${BUZZ_SANDBOX_INITIAL_POD_UID+x}" ]]; then
    if [[ -z "${BUZZ_SANDBOX_POD_UID:-}" || "${BUZZ_SANDBOX_POD_UID}" != "${BUZZ_SANDBOX_INITIAL_POD_UID:-}" ]]; then
        echo 'Sandbox workspace changed; explicit recovery is required' >&2
        exit 1
    fi
fi

if [[ -n "${BUZZ_DEVELOPER_PROFILE_ARN+x}" || -n "${BUZZ_DEVELOPER_ID+x}" ]]; then
    exec /usr/local/bin/buzz-agent-profile run -- "$@"
fi

/usr/local/bin/buzz-agent-account init

# Restore an explicitly requested checkpoint once, before the harness starts.
# The developer-profile path does the same after its bootstrap.
if [[ -n "${BUZZ_CHECKPOINT_RESTORE_KEY:-}" ]]; then
    if ! /usr/local/bin/buzz-agent-checkpoint restore-once --key "$BUZZ_CHECKPOINT_RESTORE_KEY" --workspace /home/agent >&2; then
        echo 'Checkpoint restore failed; refusing to start with an empty workspace' >&2
        exit 1
    fi
fi

# buzz-acp owns ephemeral Git setup for both native and MCP shells.
# The image init forwards termination to this direct harness child.
if [[ -n "${BUZZ_SETUP_MODE+x}" ]]; then
    # Reject harness arguments during the fixed, single-use Codex setup flow.
    [[ "$#" -eq 0 ]] || exit 1
    exec /usr/local/bin/buzz-agent-setup gate
fi
exec /usr/local/bin/buzz-agent-cache buzz-acp "$@"
