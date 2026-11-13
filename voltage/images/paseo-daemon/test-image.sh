#!/usr/bin/env bash
# Image contract test for the Paseo daemon image. Runs the container with the
# hardened flags, synthetic secrets, and no network: no relay, Buzz relay, or
# credentials are needed.
#   bash voltage/images/paseo-daemon/test-image.sh [image]
# Without an image argument, it builds paseo-daemon:local from this checkout.
set -euo pipefail
trap 'printf "FAIL: image contract exited at line %s\n" "$LINENO" >&2' ERR
IMAGE="${1:-}"
if [ -z "$IMAGE" ]; then
  IMAGE=paseo-daemon:local
  root="$(cd "$(dirname "$0")/../../.." && pwd)"
  docker build -f "$root/voltage/images/paseo-daemon/Dockerfile" -t "$IMAGE" "$root"
fi

NAME="paseo-daemon-test-$$"
containers=("$NAME" "$NAME-pa-off" "$NAME-optional" "$NAME-replacement" "$NAME-shutdown-slow")
stubs=$(mktemp -d)
# GitHub runners and the daemon use different UIDs. The read-only bind mount
# must be traversable by the container rather than inheriting mktemp's 0700.
chmod 0755 "$stubs"
cleanup() {
  docker rm -f "${containers[@]}" >/dev/null 2>&1 || true
  docker run --rm --network none --user 0:0 --entrypoint sh -v "$stubs/records:/records" "$IMAGE" -c 'rm -rf /records/slow-home' >/dev/null 2>&1 || true
  rm -rf "$stubs"
}
trap cleanup EXIT
[ -z "${KEEP:-}" ] || trap - EXIT
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# shellcheck disable=SC2054 # the tmpfs options are comma-separated
HARDENED=(--network none --cpus 1 --memory 1g --read-only --cap-drop ALL --security-opt no-new-privileges --user 1000:1000
  --tmpfs /home/paseo:uid=1000,gid=1000,mode=0700 --tmpfs /tmp)
KEYPAIR=$(docker run --rm --network none --entrypoint node "$IMAGE" -e '
  const nacl = require("/usr/local/lib/node_modules/@getpaseo/relay/node_modules/tweetnacl");
  const pair = nacl.box.keyPair();
  const b64 = (bytes) => Buffer.from(bytes).toString("base64");
  console.log(JSON.stringify({ v: 2, publicKeyB64: b64(pair.publicKey), secretKeyB64: b64(pair.secretKey) }));
')
PUBLIC_KEY=$(jq -r .publicKeyB64 <<<"$KEYPAIR")
SECRET_KEY=$(jq -r .secretKeyB64 <<<"$KEYPAIR")
PASSWORD="test-password-$RANDOM$RANDOM"
PRIVATE_KEY=$(printf '%064x' 1)
TOKEN="sk-ant-oat01-test-$RANDOM$RANDOM"
GIT_TOKEN="github_pat_test_$RANDOM$RANDOM"
AGENT=$(printf 'a%.0s' {1..64})
MANAGER=$(printf 'b%.0s' {1..64})
CHANNEL=6f1c1b2e-3d4a-4b5c-8d9e-0f1a2b3c4d5e
SECRETS=(-e "$PASSWORD" -e "$SECRET_KEY" -e "$PRIVATE_KEY" -e "$TOKEN" -e "$GIT_TOKEN")
BASE_ENV=(
  -e PASEO_PASSWORD="$PASSWORD"
  -e PASEO_SERVER_ID=srv_paseodaemontest
  -e PASEO_DAEMON_KEYPAIR="$KEYPAIR"
  -e PASEO_RELAY_ENDPOINT=relay.invalid:443
  -e BUZZ_RELAY_URL=wss://buzz.invalid
  -e BUZZ_PRIVATE_KEY="$PRIVATE_KEY"
  -e BUZZ_PASEO_OWNER="$MANAGER"
  -e BUZZ_PASEO_AGENTS='[{"id":"brokkr","label":"Brokkr","agent":"'"$AGENT"'","channel":"'"$CHANNEL"'"}]'
)
ENV=("${BASE_ENV[@]}" -e BUZZ_PASEO_RECORD_CHANNEL="$CHANNEL" -e GITHUB_READ_TOKEN="$GIT_TOKEN" -e GITHUB_READ_TOKEN_REPOSITORIES='["fixture/private"]' -e CLAUDE_CODE_OAUTH_TOKEN="$TOKEN" -e BUZZ_MANAGER_PUBKEY="$MANAGER" -e BUZZ_MANAGER_CHANNEL="$CHANNEL")

# The wrapper refuses unsafe or malformed settings before the daemon starts.
refuses() {
  local expect="$1"
  shift
  local output status=0
  output=$(docker run --rm "${HARDENED[@]}" "${ENV[@]}" "$@" "$IMAGE" 2>&1) || status=$?
  [ "$status" = 64 ] || fail "expected exit 64 for '$expect', got $status: $output"
  grep -qF "$expect" <<<"$output" || fail "expected '$expect' in: $output"
  ! grep -qF "${SECRETS[@]}" <<<"$output" || fail "a secret was printed"
}
refuses "BUZZ_RELAY_URL must start with wss://" -e BUZZ_RELAY_URL=ws://buzz.invalid
refuses "BUZZ_RELAY_URL must start with wss://" -e BUZZ_RELAY_URL=https://buzz.invalid
refuses "BUZZ_PRIVATE_KEY must be a valid secp256k1 secret" -e BUZZ_PRIVATE_KEY="$(printf '%064x' 0)"
refuses "PASEO_RELAY_USE_TLS must be true" -e PASEO_RELAY_USE_TLS=false
refuses "PASEO_RELAY_USE_TLS must be true" -e PASEO_RELAY_USE_TLS=
refuses "PASEO_RELAY_PUBLIC_USE_TLS must be true" -e PASEO_RELAY_PUBLIC_USE_TLS=0
refuses "PASEO_RELAY_ENDPOINT must be host:port" -e PASEO_RELAY_ENDPOINT=wss://relay.invalid:443
for name in PASEO_PASSWORD PASEO_SERVER_ID PASEO_DAEMON_KEYPAIR PASEO_RELAY_ENDPOINT BUZZ_RELAY_URL BUZZ_PRIVATE_KEY; do
  refuses "$name is required" -e "$name="
done
refuses "PASEO_DAEMON_KEYPAIR is not valid JSON" -e PASEO_DAEMON_KEYPAIR='{"v":2,'
refuses "PASEO_DAEMON_KEYPAIR must be" -e PASEO_DAEMON_KEYPAIR="$(jq -c '.v = 1' <<<"$KEYPAIR")"
refuses "PASEO_DAEMON_KEYPAIR must be" -e PASEO_DAEMON_KEYPAIR="$(jq -c '.publicKeyB64 = "AAAA"' <<<"$KEYPAIR")"
# A public key that does not belong to the secret key would pair clients to a
# daemon that cannot decrypt their traffic.
refuses "PASEO_DAEMON_KEYPAIR must be" -e PASEO_DAEMON_KEYPAIR="$(jq -c --arg k "$SECRET_KEY" '.publicKeyB64 = $k' <<<"$KEYPAIR")"
refuses "BUZZ_PASEO_AGENTS[0].id must match" -e BUZZ_PASEO_AGENTS='[{"id":"Bad_Id","label":"x","agent":"'"$AGENT"'","channel":"'"$CHANNEL"'"}]'
refuses 'BUZZ_PASEO_AGENTS[0].id "claude" is reserved' -e BUZZ_PASEO_AGENTS='[{"id":"claude","label":"x","agent":"'"$AGENT"'","channel":"'"$CHANNEL"'"}]'
refuses "BUZZ_PASEO_AGENTS[0].channel must be a UUID" -e BUZZ_PASEO_AGENTS='[{"id":"brokkr","label":"x","agent":"'"$AGENT"'","channel":"general"}]'
refuses "BUZZ_PASEO_OWNER is required" -e BUZZ_PASEO_OWNER=
refuses "BUZZ_PASEO_RECORD_CHANNEL must be a UUID" -e BUZZ_PASEO_RECORD_CHANNEL=general
refuses "set both BUZZ_MANAGER_PUBKEY and BUZZ_MANAGER_CHANNEL" -e BUZZ_MANAGER_CHANNEL=
refuses "invalid GITHUB_READ_TOKEN_REPOSITORIES" -e GITHUB_READ_TOKEN_REPOSITORIES='["fixture/private","Fixture/Private"]'
refuses "invalid GITHUB_READ_TOKEN_REPOSITORIES" -e GITHUB_READ_TOKEN_REPOSITORIES='["fixture/../private"]'

wait_running() {
  local name="$1" status=""
  for _ in $(seq 60); do
    status=$(docker exec "$name" paseo status --json 2>/dev/null) && jq -e ' .localDaemon == "running" and (.providers | type == "array") and (.relay | type == "object") ' <<<"$status" >/dev/null && break
    [ "$(docker inspect -f '{{.State.Running}}' "$name")" = true ] || { docker logs "$name" >&2; fail "$name exited"; }
    status=""
    sleep 1
  done
  [ -n "$status" ] || { docker logs "$name" >&2; fail "$name did not report running"; }
  printf '%s' "$status"
}

# Without the model token, the daemon offers only the Buzz remote providers.
docker run -d --name "$NAME-pa-off" "${HARDENED[@]}" "${BASE_ENV[@]}" "$IMAGE" >/dev/null
status=$(wait_running "$NAME-pa-off")
[ "$(jq -c '[.providers[].provider]' <<<"$status")" = '["brokkr"]' ] || fail "providers without the token: $status"
docker rm -f "$NAME-pa-off" >/dev/null

# Every optional-missing combination starts without a model or remote provider.
for agents in missing '[]'; do
  if [ "$agents" = missing ]; then agent_env=(); else agent_env=(-e BUZZ_PASEO_AGENTS="$agents"); fi
  docker run -d --name "$NAME-optional" "${HARDENED[@]}" "${BASE_ENV[@]}" -e BUZZ_PASEO_AGENTS= "${agent_env[@]}" "$IMAGE" >/dev/null
  optional_status=$(wait_running "$NAME-optional")
  [ "$(jq '.providers | length' <<<"$optional_status")" = 0 ] || fail "optional providers: $optional_status"
  docker rm -f "$NAME-optional" >/dev/null
done
# Codex remains disabled even when a token is accidentally supplied.
docker run -d --name "$NAME-optional" "${HARDENED[@]}" "${BASE_ENV[@]}" -e CODEX_ACCESS_TOKEN=at-test-token "$IMAGE" >/dev/null
optional_status=$(wait_running "$NAME-optional")
[ "$(jq -c '[.providers[].provider]' <<<"$optional_status")" = '["brokkr"]' ] || fail "Codex was enabled: $optional_status"
docker rm -f "$NAME-optional" >/dev/null
refuses "GITHUB_READ_TOKEN must be a fine-grained GitHub token" -e GITHUB_READ_TOKEN=gho-broad-token

# Stubs record how the daemon launches the PA and the bridge. They sit first
# on PATH, so the daemon resolves them in place of the image binaries.
cat >"$stubs/claude" <<'STUB'
#!/bin/sh
case "$*" in
  *--version*) echo "2.1.246 (Claude Code)"; exit 0 ;;
  *"auth status"*) echo '{"loggedIn":true}'; exit 0 ;;
esac
# Metadata generation also invokes Claude. Keep only the real assistant launch.
case "$*" in *--disallowedTools* | *--mcp-config*) ;; *) exit 1 ;; esac
printf '%s\n' "$@" >/tmp/claude.args
# The SDK sends the system prompt in its first stdin message.
timeout 10 head -n 1 >/tmp/claude.stdin
env >/tmp/claude.env
exit 1
STUB
cat >"$stubs/buzz" <<'STUB'
#!/bin/sh
if [ "$1 $2" = "messages send" ]; then
  sleep "${RECORD_TEST_DELAY:-0}"
  cat >>"$RECORD_TEST_OUTPUT"
  printf "\n" >>"$RECORD_TEST_OUTPUT"
  chmod 0644 "$RECORD_TEST_OUTPUT"
  printf '{"accepted":true,"event_id":"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"}\n'
  exit 0
fi
printf '%s\n' "$@" >/tmp/buzz.args
env >/tmp/buzz.env
exit 1
STUB
chmod 0755 "$stubs/claude" "$stubs/buzz"
# Exercise a different UID even on developer machines where the daemon and
# host happen to share UID 1000. Without directory traversal the real provider
# would fall back to the image binary instead of these launch probes.
docker run --rm --network none --user 65534:65534 --entrypoint sh \
  -v "$stubs:/tmp/stubs:ro" "$IMAGE" \
  -c 'test -x /tmp/stubs/buzz && test -x /tmp/stubs/claude' || fail "provider stubs are inaccessible to another UID"
mkdir "$stubs/records"
chmod 0777 "$stubs/records"
STUB_PATH=/tmp/stubs:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Startup with the hardened flags.
docker run -d --name "$NAME" "${HARDENED[@]}" "${ENV[@]}" -v "$stubs:/tmp/stubs:ro" -v "$stubs/records:/tmp/records" -e PATH="$STUB_PATH" -e RECORD_TEST_OUTPUT=/tmp/records/turn "$IMAGE" >/dev/null
status=$(wait_running "$NAME")
[ "$(docker exec "$NAME" id -u)" = 1000 ] || fail "not uid 1000"
[ "$(jq -r .serverId <<<"$status")" = srv_paseodaemontest ] || fail "server id: $status"
[ "$(jq -r .listen <<<"$status")" = 127.0.0.1:6767 ] || fail "listener is not loopback: $status"
jq -e '.relay | .enabled and .useTls and .publicUseTls and .endpoint == "relay.invalid:443"' <<<"$status" >/dev/null ||
  fail "relay settings: $status"
# The image's own claude is the Claude Code binary bundled with the daemon.
docker exec -e PATH=/usr/local/bin:/usr/bin:/bin "$NAME" claude --version | grep -q '(Claude Code)' ||
  fail "the image claude is not Claude Code"

# The PA and the Buzz agent are offered; the other bundled providers are not.
[ "$(jq -c '[.providers[].provider] | sort' <<<"$status")" = '["brokkr","claude"]' ] || fail "enabled providers: $status"
docker exec "$NAME" paseo provider ls --json |
  jq -e '(map(select(.enabled == "Enabled")) | map(.provider) | sort) == ["brokkr", "claude"] and (map(.provider) | index("codex"))' >/dev/null ||
  fail "provider list: $(docker exec "$NAME" paseo provider ls --json)"

# The pairing offer carries the provided identity.
offer=$(docker exec "$NAME" paseo daemon pair --json | jq -r .url | sed 's/.*#offer=//')
docker exec "$NAME" node -e '
  const offer = JSON.parse(Buffer.from(process.argv[1], "base64url").toString());
  const ok = offer.serverId === "srv_paseodaemontest" && offer.daemonPublicKeyB64 === process.argv[2] &&
    offer.relay?.endpoint === "relay.invalid:443" && offer.relay?.useTls === true;
  if (!ok) { console.error(JSON.stringify({ ...offer, daemonPublicKeyB64: undefined })); process.exit(1); }
' "$offer" "$PUBLIC_KEY" || fail "pairing offer does not match the provided identity"

# The bridge gets the Buzz relay and key, and no Paseo password or model token.
docker exec -w /home/paseo/assistant "$NAME" paseo run --no-wait --provider brokkr "hello" >/dev/null 2>&1 || true
for _ in $(seq 30); do docker exec "$NAME" test -s /tmp/buzz.env && break; sleep 1; done
[ "$(docker exec "$NAME" cat /tmp/buzz.args | paste -sd' ')" = "acp-bridge --agent $AGENT --channel $CHANNEL --owner $MANAGER" ] ||
  fail "bridge command: $(docker exec "$NAME" cat /tmp/buzz.args 2>&1)"
bridge_env=$(docker exec "$NAME" cat /tmp/buzz.env)
grep -qx "BUZZ_PRIVATE_KEY=$PRIVATE_KEY" <<<"$bridge_env" || fail "the bridge has no BUZZ_PRIVATE_KEY"
grep -qx "BUZZ_RELAY_URL=wss://buzz.invalid" <<<"$bridge_env" || fail "the bridge has no BUZZ_RELAY_URL"
! grep -qF -e "$PASSWORD" -e "$TOKEN" -e "$SECRET_KEY" -e "$GIT_TOKEN" <<<"$bridge_env" || fail "the bridge received a secret it does not use"

# The PA gets its token, the Buzz key, read-only file tools, and the Paseo tools,
# without write/web/schedule tools or the Paseo password.
docker exec -w /home/paseo/assistant "$NAME" paseo run --no-wait --provider claude/sonnet "hello" >/dev/null 2>&1 || true
for _ in $(seq 30); do docker exec "$NAME" test -s /tmp/claude.env && break; sleep 1; done
claude_env=$(docker exec "$NAME" cat /tmp/claude.env 2>&1)
grep -qx "CLAUDE_CODE_OAUTH_TOKEN=$TOKEN" <<<"$claude_env" || fail "the PA has no model token"
grep -qx "BUZZ_PRIVATE_KEY=$PRIVATE_KEY" <<<"$claude_env" || fail "the PA has no BUZZ_PRIVATE_KEY"
grep -qx "GITHUB_READ_TOKEN=$GIT_TOKEN" <<<"$claude_env" || fail "the PA has no narrow GitHub read token"
! grep -qF -e "$PASSWORD" -e "$SECRET_KEY" <<<"$claude_env" || fail "the PA received the Paseo password or keypair"
claude_args=$(docker exec "$NAME" cat /tmp/claude.args)
disallowed=$(grep -A1 -x -- --disallowedTools <<<"$claude_args" | tail -1)
for tool in Write Edit WebFetch WebSearch; do
  tr ',' '\n' <<<"$disallowed" | grep -qx "$tool" || fail "the PA can use $tool: $disallowed"
done
for tool in Read Glob Grep; do
  ! tr ',' '\n' <<<"$disallowed" | grep -qx "$tool" || fail "read-only file tool is disabled: $tool"
done
init=$(docker exec "$NAME" cat /tmp/claude.stdin)
grep -qF "personal assistant of one developer" <<<"$init" || fail "the PA instructions are missing: ${init:0:300}"
grep -qF "$MANAGER" <<<"$init" && grep -qF "$CHANNEL" <<<"$init" || fail "the manager is missing from the PA instructions"
mcp_config=$(grep -A1 -x -- --mcp-config <<<"$claude_args" | tail -1)
tools=$(docker exec "$NAME" node -e '
  const server = JSON.parse(process.argv[1]).mcpServers.paseo;
  const headers = { ...server.headers, "content-type": "application/json", accept: "application/json, text/event-stream" };
  const call = async (body, session) => {
    const response = await fetch(server.url, { method: "POST", headers: { ...headers, ...(session ? { "mcp-session-id": session } : {}) }, body: JSON.stringify(body) });
    const text = await response.text();
    const data = text.startsWith("{") ? text : text.split("\n").filter((line) => line.startsWith("data:")).map((line) => line.slice(5)).pop();
    return { session: response.headers.get("mcp-session-id"), result: JSON.parse(data).result };
  };
  (async () => {
    const init = await call({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "test", version: "0" } } });
    await fetch(server.url, { method: "POST", headers: { ...headers, ...(init.session ? { "mcp-session-id": init.session } : {}) }, body: JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) });
    const list = await call({ jsonrpc: "2.0", id: 2, method: "tools/list" }, init.session);
    console.log(list.result.tools.map((tool) => tool.name).sort().join("\n"));
  })().catch((error) => { console.error(error); process.exit(1); });
' "$mcp_config") || fail "could not list the PA Paseo tools"
for tool in create_agent send_agent_prompt get_agent_status list_providers; do
  grep -qx "$tool" <<<"$tools" || fail "the PA lacks the Paseo tool $tool: $tools"
done
! grep -E '^(create_terminal|send_terminal_keys|start_workspace_script|create_workspace|create_schedule|create_heartbeat|run_schedule_once)$' <<<"$tools" ||
  fail "the PA has a Paseo tool that runs commands: $tools"

# Terminals are off for clients as well.
workspace=$(docker exec "$NAME" paseo workspace ls --json | jq -r '.[0].workspaceId // .[0].id // empty')
[ -n "$workspace" ] || fail "no workspace: $(docker exec "$NAME" paseo workspace ls --json)"
terminal=$(docker exec "$NAME" paseo terminal create --workspace "$workspace" --cwd /home/paseo/assistant --json 2>&1 || true)
grep -q TERMINAL_CREATE_FAILED <<<"$terminal" || fail "a terminal was created: $terminal"

# Secrets stay out of logs and out of the daemon environment.
logs=$(docker logs "$NAME" 2>&1)
! grep -qF "${SECRETS[@]}" <<<"$logs" || fail "a secret was logged"
! docker exec "$NAME" sh -c 'tr "\0" "\n" < /proc/1/environ' | grep -q '^PASEO_DAEMON_KEYPAIR=' ||
  fail "PASEO_DAEMON_KEYPAIR reached the daemon environment"
[ "$(docker exec "$NAME" stat -c %a /home/paseo/.paseo/daemon-keypair.json)" = 600 ] || fail "keypair file mode"
[ "$(docker exec "$NAME" stat -c %a /home/paseo/.paseo/config.json)" = 600 ] || fail "config file mode"
docker exec "$NAME" cat /home/paseo/.claude/settings.json | jq -e '.permissions.disableBypassPermissionsMode == "disable"' >/dev/null || fail "Claude bypass protection"
docker exec "$NAME" cat /home/paseo/.claude/settings.json |
  jq -e '[.permissions.deny[]] | (index("Read(//proc/**)") != null) and (index("Read(~/.paseo/**)") != null)' >/dev/null ||
  fail "Claude can read process environments or the pairing keypair"
docker exec "$NAME" cat /home/paseo/.paseo/config.json |
  jq -e '[.. | .allow? // empty | .[]] | (map(select(test("messages send|^Read$"))) | length) == 0' >/dev/null ||
  fail "posting or unrestricted reads are pre-approved"
docker exec "$NAME" node -e '
  const assert=require("node:assert/strict"),cp=require("node:child_process");
  const lookup=(requested)=>cp.spawnSync("git",["credential","fill"],{env:{...process.env,GIT_TERMINAL_PROMPT:"0"},input:`protocol=https\nhost=github.com\npath=${requested}\n\n`,encoding:"utf8",timeout:2000});
  const allowed=lookup("fixture/private.git");
  assert.equal(allowed.status,0);assert.ok(allowed.stdout.includes(`password=${process.env.GITHUB_READ_TOKEN}\n`));
  const refused=lookup("fixture/public.git");assert.notEqual(refused.status,0);assert.equal(refused.stdout,"");
' || fail "packaged Git credential repository binding"

# Nothing is written outside the tmpfs mounts.
[ "$(docker inspect -f '{{.HostConfig.ReadonlyRootfs}}' "$NAME")" = true ] || fail "root filesystem is writable"
changes=$(docker diff "$NAME")
[ -z "$changes" ] || fail "writes outside tmpfs: $changes"

# Runtime package installers, toolchains, scanners and Codex are absent.
docker exec "$NAME" sh -c '
  for command in codex npm npx yarn corepack apt apt-get dpkg cargo rustc gcc cc make cmake python3 pip gitleaks; do
    if command -v "$command" >/dev/null 2>&1; then echo "unexpected runtime tool: $command" >&2; exit 1; fi
  done
  command -v git >/dev/null && command -v node >/dev/null
' || fail "runtime tool inventory"
docker exec "$NAME" node -e '
  const fs = require("node:fs");
  const config = JSON.parse(fs.readFileSync("/home/paseo/.paseo/config.json"));
  if (config.daemon.terminalProfiles || config.agents.providers.codex.enabled) process.exit(1);
  const allow = config.agents.providers.claude.options.settings.permissions.allow;
  if (allow.includes("Bash(buzz:*)") || allow.some((rule) => /workflows (create|update|trigger)/.test(rule))) process.exit(1);
  for (const verb of ["list", "delete"]) if (!allow.includes(`Bash(buzz workflows ${verb}:*)`)) process.exit(1);
  const models = config.agents.providers.brokkr.models;
  if (models?.length !== 1 || models[0].isDefault !== true || models[0].label !== "Brokkr") process.exit(1);
' || fail "deferred terminal profile or Codex is enabled, or a Buzz provider lacks its preselected model"

# The bundled plugin compiles and starts in the unmodified daemon.
docker exec "$NAME" paseo plugin ls --json >"$stubs/plugins.json"
jq -e 'any(.[]; .id == "buzz-recorder" and .status == "running")' "$stubs/plugins.json" >/dev/null || fail "recorder plugin did not load"
node "$(dirname "$0")/test-recorder.mjs"
node "$(dirname "$0")/test-code.mjs"
node "$(dirname "$0")/test-workspaces.mjs"
node "$(dirname "$0")/test-code-git.mjs"
# The fake Git executable needs an owned executable bind: /tmp stays noexec.
docker run --rm "${HARDENED[@]}" --entrypoint node -e PASEO_CODE_HELPER=/usr/local/bin/paseo-code -e TMPDIR=/test-work \
  -v "$stubs/records:/test-work" -v "$(cd "$(dirname "$0")" && pwd)/test-code.mjs:/test-code.mjs:ro" "$IMAGE" /test-code.mjs
docker run --rm "${HARDENED[@]}" --entrypoint node -e PASEO_CODE_HELPER=/usr/local/bin/paseo-code \
  -v "$(cd "$(dirname "$0")" && pwd)/test-code-git.mjs:/test-code-git.mjs:ro" "$IMAGE" /test-code-git.mjs

docker stats --no-stream --format '  {{.MemUsage}} container memory' "$NAME"
# Test the packaged scrubber, including opaque credentials and public IDs.
docker exec "$NAME" node --input-type=module -e '
  import assert from "node:assert/strict";
  import { scrub } from "/opt/paseo-buzz/recorder/server/recorder.mjs";
  const pubkey = "a1".repeat(32);
  assert.equal(scrub(pubkey, {}), pubkey);
  for (const label of ["private_key", "secret", "token"]) assert.match(scrub(JSON.stringify({ [label]: pubkey }), {}), /omitted/);
  const publicFields = JSON.stringify({ pubkey, event_id: pubkey });
  assert.equal(scrub(publicFields, {}), publicFields);
  assert.match(scrub(Buffer.alloc(32, 0x87).toString("base64"), {}), /omitted/);
' || fail "packaged scrubber"

# A real SIGTERM reaches the plugin while a durable intent remains. The
# publisher must acknowledge it before the daemon exits within the grace.
docker exec "$NAME" node -e '
  const fs = require("node:fs");
  fs.mkdirSync("/home/paseo/.paseo/buzz-recording", { recursive: true });
  const id = "e".repeat(64);
  fs.writeFileSync(`/home/paseo/.paseo/buzz-recording/${id}.json`, JSON.stringify({ version: 2, id, session: id, order: 1, next: 0,
    messages: [{ content: "final shutdown turn", seqEnd: 1, role: "developer", part: 1, parts: 1, entry: id,
      metadata: { session: id, entry: id, role: "developer", part: 1, parts: 1, sequence: 1, created_at: 100 } }] }));
'
# Recording was enabled at startup through this test-specific channel.
started=$SECONDS
docker stop --time 30 "$NAME" >/dev/null
[ $((SECONDS - started)) -lt 25 ] || fail "shutdown exceeded the recorder grace budget"
grep -qx "final shutdown turn" "$stubs/records/turn" || fail "SIGTERM did not flush the durable turn"
docker rm "$NAME" >/dev/null

# A slow publisher cannot outlive upstream's plugin shutdown window. Keep
# this test's home on an owned bind so the unacknowledged intent is inspectable.
docker run -d --name "$NAME-shutdown-slow" "${HARDENED[@]}" "${BASE_ENV[@]}" \
  -e BUZZ_PASEO_RECORD_CHANNEL="$CHANNEL" -e PASEO_HOME=/tmp/records/slow-home \
  -v "$stubs:/tmp/stubs:ro" -v "$stubs/records:/tmp/records" -e PATH="$STUB_PATH" \
  -e RECORD_TEST_OUTPUT=/tmp/records/slow-ack -e RECORD_TEST_DELAY=5 "$IMAGE" >/dev/null
wait_running "$NAME-shutdown-slow" >/dev/null
docker exec "$NAME-shutdown-slow" paseo plugin ls --json | jq -e 'any(.[]; .id == "buzz-recorder" and .status == "running")' >/dev/null || fail "slow-case plugin not loaded"
docker exec "$NAME-shutdown-slow" node -e '
  const fs = require("node:fs"); const id = "d".repeat(64);
  const directory = `${process.env.PASEO_HOME}/buzz-recording`;
  fs.mkdirSync(directory, { recursive: true });
  fs.writeFileSync(`${directory}/${id}.json`, JSON.stringify({ version: 2, id, session: id, order: 1, next: 0,
    messages: [{ content: "unacknowledged slow turn", seqEnd: 1, role: "developer", part: 1, parts: 1, entry: id,
      metadata: { session: id, entry: id, role: "developer", part: 1, parts: 1, sequence: 1, created_at: 100 } }] }));
'
started=$SECONDS
docker stop --time 10 "$NAME-shutdown-slow" >/dev/null
[ $((SECONDS - started)) -lt 8 ] || fail "slow publisher exceeded bounded shutdown"
[ ! -e "$stubs/records/slow-ack" ] || fail "slow publisher survived shutdown"
docker run --rm --network none --user 0:0 --entrypoint sh -v "$stubs/records:/records:ro" "$IMAGE" \
  -c 'test -f /records/slow-home/buzz-recording/dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd.json' || fail "slow publisher lost unacknowledged intent"
docker rm "$NAME-shutdown-slow" >/dev/null

# Fresh home, same synthetic Secret: pairing identity must remain unchanged.
docker run -d --name "$NAME-replacement" "${HARDENED[@]}" "${ENV[@]}" "$IMAGE" >/dev/null
replacement_status=$(wait_running "$NAME-replacement" "$NAME-shutdown-slow")
[ "$(jq -r .serverId <<<"$replacement_status")" = srv_paseodaemontest ] || fail "replacement server identity changed"
replacement_offer=$(docker exec "$NAME-replacement" paseo daemon pair --json | jq -r .url | sed 's/.*#offer=//')
docker exec "$NAME-replacement" node -e '
  const offer = JSON.parse(Buffer.from(process.argv[1], "base64url").toString());
  if (offer.daemonPublicKeyB64 !== process.argv[2]) process.exit(1);
' "$replacement_offer" "$PUBLIC_KEY" || fail "replacement pairing key changed"
printf 'PASS: Paseo daemon image contract (%s)\n' "$IMAGE"
