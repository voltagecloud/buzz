#!/usr/bin/env bash
# No account sign-in or model requests: prove the packaged adapters initialize
# through the real Buzz harness against an empty, container-local HOME.
set -euo pipefail
IMAGE="${1:-buzz-agent:local}"
docker run --rm --entrypoint /bin/bash "$IMAGE" -ceu '
  test "$(id -u)" = 10001
  test "$(id -g)" = 10001
  test "$HOME" = /home/agent
  test "$(pwd)" = /home/agent
  for binary in buzz-acp buzz-dev-mcp codex-acp claude-agent-acp codex claude kubectl; do
    command -v "$binary" >/dev/null
  done
  codex --version
  claude --version
  kubectl version --client
  codex login --help | grep -q -- --device-auth
  claude setup-token --help >/dev/null
  # Startup appends the sub-agent guide to the harness instructions as this user.
  test -r /etc/buzz-agent/subagents.md
  test -r /etc/buzz-agent/cluster.md
  ! test -e /home/agent/.codex/auth.json
  ! test -e /home/agent/.claude/.credentials.json
'
for adapter in codex-acp claude-agent-acp; do
  docker run --rm "$IMAGE" auth-methods --agent-command "$adapter" --agent-args '' --json
done
# Shadow HOME like the provider volume. Runtime installs must remain available.
docker run --rm --tmpfs /home/agent:uid=10001,gid=10001,mode=0700 \
  --entrypoint /bin/bash "$IMAGE" -ceu '
    test -w "$HOME"
    codex --version >/dev/null
    claude --version >/dev/null
    command -v codex-acp claude-agent-acp buzz-dev-mcp >/dev/null
  '
printf 'PASS: agent adapter packaging and unauthenticated ACP initialization (%s)\n' "$IMAGE"
