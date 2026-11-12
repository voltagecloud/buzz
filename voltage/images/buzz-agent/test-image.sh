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
  for binary in buzz-acp buzz-dev-mcp codex-acp claude-agent-acp codex claude kubectl buzz-subagent buzz-agent-review; do
    command -v "$binary" >/dev/null
  done
  buzz-subagent review --help >/dev/null
  codex --version
  claude --version
  kubectl version --client
  gh stack --help >/dev/null
  codex login --help | grep -q -- --device-auth
  claude setup-token --help >/dev/null
  # Startup appends the sub-agent guide to the harness instructions as this user.
  test -r /etc/buzz-agent/subagents.md
  test -r /etc/buzz-agent/cluster.md
  ! test -e /home/agent/.codex/auth.json
  ! test -e /home/agent/.claude/.credentials.json
  # Engineering skills (select-skills.sh): read-only, each with SKILL.md, none
  # of the excluded ones. Empty in pull request and local builds.
  test -r /opt/engineering-skills/REVISION
  ! test -w /opt/engineering-skills/skills
  for skill in /opt/engineering-skills/skills/*/; do
    [ -d "$skill" ] || continue
    test -r "$skill/SKILL.md"
  done
  ! test -e /opt/engineering-skills/skills/cli-demo-video
  # walkthrough-video tools, outside HOME and read-only for the agent.
  ffmpeg -version >/dev/null
  test "$WALKTHROUGH_CACHE" = /opt/walkthrough-video
  test -x "$WALKTHROUGH_CACHE/venv/bin/piper"
  ! test -w "$WALKTHROUGH_CACHE/voices"
  for voice in en_GB-alan-medium en_US-lessac-medium; do
    test -r "$WALKTHROUGH_CACHE/voices/$voice.onnx"
    test -r "$WALKTHROUGH_CACHE/voices/$voice.onnx.json"
  done
'
# Published images must carry skills (the publish job sets REQUIRE_SKILLS=1).
if [ "${REQUIRE_SKILLS:-0}" = 1 ]; then
  docker run --rm --entrypoint /bin/sh "$IMAGE" -ceu '
    grep -qE "^[0-9a-f]{40}$" /opt/engineering-skills/REVISION
    ls /opt/engineering-skills/skills/*/SKILL.md >/dev/null
  '
fi
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
python3 "$(dirname "$0")/test-review.py" "$IMAGE"
printf 'PASS: agent adapter packaging and unauthenticated ACP initialization (%s)\n' "$IMAGE"
