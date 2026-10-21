#!/usr/bin/env bash
# Throwaway, isolated Buzz relay for developing buzz-gpui.
#
#   scripts/test-relay.sh up       build (if needed), start deps, migrate, start relay
#   scripts/test-relay.sh seed     identities, members, channels, messages (idempotent)
#   scripts/test-relay.sh verify   WS (NIP-42) + HTTP (NIP-98) smoke checks
#   scripts/test-relay.sh status   show deps/relay state
#   scripts/test-relay.sh logs     tail relay log (`logs deps` for docker logs)
#   scripts/test-relay.sh restart  restart only the relay process
#   scripts/test-relay.sh down     stop relay and deps (data kept)
#   scripts/test-relay.sh wipe     down + delete DB/minio data and seed.json
#   scripts/test-relay.sh up-tls   also run a TLS relay at wss://localhost:53443
#   scripts/test-relay.sh down-tls stop only the TLS relay and its TLS proxy
#
# Everything lives in .test-relay/ (gitignored), or in $TEST_RELAY_DIR if set
# (to share one test relay between worktrees). See scripts/TEST-RELAY.md.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STATE="${TEST_RELAY_DIR:-${ROOT}/.test-relay}"
SRC="${STATE}/buzz"
BUZZ_REPO="https://github.com/block/buzz"
BUZZ_REV="5142cf6b2a0f44686eeda1ed07905ee3560fd78e"
PROJECT="buzz-gpui-test"
COMPOSE_FILE="${ROOT}/scripts/test-relay.compose.yml"

RELAY_PORT=53000
HEALTH_PORT=58080
METRICS_PORT=59102
PG_PORT=55432
REDIS_PORT=56379
MINIO_PORT=59000

# The community is keyed by this exact authority. Clients must connect with
# Host: 127.0.0.1:53000 ("localhost:53000" is a different, unmapped host).
COMMUNITY_HOST="127.0.0.1:${RELAY_PORT}"
WS_URL="ws://${COMMUNITY_HOST}"
HTTP_URL="http://${COMMUNITY_HOST}"
DATABASE_URL="postgres://buzz:buzz_dev@127.0.0.1:${PG_PORT}/buzz"
REDIS_URL="redis://127.0.0.1:${REDIS_PORT}"

IDENTITIES="${STATE}/identities.json"
SEED_FILE="${STATE}/seed.json"
RELAY_ENV="${STATE}/relay.env"
RELAY_LOG="${STATE}/relay.log"
RELAY_PID="${STATE}/relay.pid"
BIN="${SRC}/target/debug"

# TLS relay (`up-tls`): a second relay process configured for the origin
# wss://localhost:53443, with its own database and Redis DB, behind a socat
# TLS terminator. The relay derives the NIP-42 `relay` and NIP-98 `u` scheme
# from RELAY_URL, so one process cannot serve both ws:// and wss://.
TLS_PORT=53443
TLS_BACKEND_PORT=53001
TLS_HEALTH_PORT=58081
TLS_METRICS_PORT=59103
TLS_HOST="localhost:${TLS_PORT}"
TLS_WS_URL="wss://${TLS_HOST}"
TLS_HTTP_URL="https://${TLS_HOST}"
TLS_DB="buzz_tls"
TLS_DIR="${STATE}/tls"
TLS_CA="${TLS_DIR}/ca.pem"
TLS_BUNDLE="${TLS_DIR}/ca-bundle.pem"
TLS_STATE_FILE="${TLS_DIR}/relay.json"
TLS_RELAY_LOG="${TLS_DIR}/relay.log"
TLS_RELAY_PID="${TLS_DIR}/relay.pid"
TLS_PROXY_LOG="${TLS_DIR}/proxy.log"
TLS_PROXY_PID="${TLS_DIR}/proxy.pid"

export TEST_RELAY_STATE="${STATE}"
export TEST_RELAY_UID="$(id -u)"
export TEST_RELAY_GID="$(id -g)"

log() { printf '\033[0;34m[test-relay]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[0;31m[test-relay]\033[0m %s\n' "$*" >&2; exit 1; }

compose() { docker compose -p "${PROJECT}" -f "${COMPOSE_FILE}" "$@"; }

# ── tools ────────────────────────────────────────────────────────────────────

NAK=""
nak() {
  if [[ -z "${NAK}" ]]; then
    if type -P nak >/dev/null 2>&1; then
      NAK="$(type -P nak)"
    else
      NAK="$(nix build nixpkgs#nak --no-link --print-out-paths 2>/dev/null)/bin/nak"
    fi
    [[ -x "${NAK}" ]] || die "could not get nak (nix build nixpkgs#nak failed)"
  fi
  "${NAK}" "$@" </dev/null
}

ident() { jq -r --arg n "$1" --arg f "$2" '.identities[$n][$f]' "${IDENTITIES}"; }

# ── source + build ───────────────────────────────────────────────────────────

ensure_src() {
  if [[ ! -d "${SRC}/.git" ]]; then
    log "Fetching ${BUZZ_REPO} @ ${BUZZ_REV:0:12}..."
    mkdir -p "${SRC}"
    git -C "${SRC}" init -q
    git -C "${SRC}" remote add origin "${BUZZ_REPO}"
    git -C "${SRC}" fetch -q --depth 1 origin "${BUZZ_REV}"
    git -C "${SRC}" checkout -q FETCH_HEAD
  fi
  # Upstream hardcodes 0.0.0.0 for the health and metrics listeners; keep the
  # test relay loopback-only. (Idempotent local patch to the throwaway clone.)
  sed -i 's/bind(("0\.0\.0\.0", config\.health_port))/bind(("127.0.0.1", config.health_port))/' \
    "${SRC}/crates/buzz-relay/src/main.rs"
  sed -i 's/with_http_listener((\[0, 0, 0, 0\], port))/with_http_listener(([127, 0, 0, 1], port))/' \
    "${SRC}/crates/buzz-relay/src/metrics.rs"
  grep -q 'bind(("127.0.0.1", config.health_port))' "${SRC}/crates/buzz-relay/src/main.rs" \
    || die "health-listener loopback patch did not apply"
  grep -q 'with_http_listener((\[127, 0, 0, 1\], port))' "${SRC}/crates/buzz-relay/src/metrics.rs" \
    || die "metrics-listener loopback patch did not apply"
}

build() {
  ensure_src
  log "Building buzz-relay, buzz-admin, buzz (cli), buzz-test-cli (dev profile)..."
  # rust-toolchain.toml pins 1.95; the system toolchain (newer) builds it fine.
  # cmake is needed by audiopus_sys; the rest are belt and braces.
  (cd "${SRC}" && nix shell nixpkgs#cmake nixpkgs#pkg-config nixpkgs#protobuf --command \
    cargo build -q -p buzz-relay -p buzz-admin -p buzz-cli -p buzz-test-client) \
    >>"${STATE}/build.log" 2>&1 || die "cargo build failed; see ${STATE}/build.log"
}

# ── identities + relay env ───────────────────────────────────────────────────

ensure_identities() {
  [[ -f "${IDENTITIES}" ]] && return 0
  log "Generating throwaway identities -> ${IDENTITIES}"
  local entries=() name role sk pk
  for spec in alice:owner bob:member carol:member; do
    name="${spec%%:*}"; role="${spec##*:}"
    sk="$(nak key generate)"
    pk="$(nak key public "${sk}")"
    entries+=("$(jq -n --arg name "${name}" --arg role "${role}" --arg sk "${sk}" --arg pk "${pk}" \
      --arg nsec "$(nak encode nsec "${sk}")" --arg npub "$(nak encode npub "${pk}")" \
      '{($name): {role: $role, display_name: ($name | (.[:1] | ascii_upcase) + .[1:]),
                  sk_hex: $sk, nsec: $nsec, pk_hex: $pk, npub: $npub}}')")
  done
  printf '%s\n' "${entries[@]}" | jq -s \
    --arg ws "${WS_URL}" --arg http "${HTTP_URL}" --arg host "${COMMUNITY_HOST}" \
    '{note: "TEST-ONLY throwaway keys for the local buzz-gpui test relay",
      relay_ws_url: $ws, relay_http_url: $http, community_host: $host,
      identities: add}' > "${IDENTITIES}"
  chmod 600 "${IDENTITIES}"
}

ensure_relay_env() {
  [[ -f "${RELAY_ENV}" ]] && return 0
  log "Writing ${RELAY_ENV} (stable relay signing key)"
  umask 077
  cat > "${RELAY_ENV}" <<EOF
BUZZ_RELAY_PRIVATE_KEY=$(nak key generate)
EOF
}

relay_env() {
  # shellcheck disable=SC1090
  source "${RELAY_ENV}"
  cat <<EOF
DATABASE_URL=${DATABASE_URL}
REDIS_URL=${REDIS_URL}
RELAY_URL=${WS_URL}
BUZZ_BIND_ADDR=127.0.0.1:${RELAY_PORT}
BUZZ_HEALTH_PORT=${HEALTH_PORT}
BUZZ_METRICS_PORT=${METRICS_PORT}
BUZZ_RELAY_PRIVATE_KEY=${BUZZ_RELAY_PRIVATE_KEY}
BUZZ_REQUIRE_RELAY_MEMBERSHIP=true
RELAY_OWNER_PUBKEY=$(ident alice pk_hex)
BUZZ_REQUIRE_AUTH_TOKEN=true
BUZZ_RECONCILE_CHANNELS=true
BUZZ_PUSH_ENABLED=false
BUZZ_GIT_CONFORMANCE_PROBE=false
BUZZ_GIT_REPO_PATH=${STATE}/git-repos
BUZZ_S3_ENDPOINT=http://127.0.0.1:${MINIO_PORT}
BUZZ_S3_ACCESS_KEY=buzz_dev
BUZZ_S3_SECRET_KEY=buzz_dev_secret
BUZZ_S3_BUCKET=buzz-media
BUZZ_S3_REGION=us-east-1
BUZZ_S3_ADDRESSING_STYLE=path
RUST_LOG=${RUST_LOG:-info,buzz_relay=debug,buzz_auth=debug}
EOF
}

# with_env_of <env-function> <command...>
with_env_of() {
  local envs=() source="$1"; shift
  mapfile -t envs < <("${source}")
  env "${envs[@]}" "$@"
}

with_relay_env() { with_env_of relay_env "$@"; }

# ── deps + relay lifecycle ───────────────────────────────────────────────────

pid_running() { [[ -f "$1" ]] && kill -0 "$(cat "$1")" 2>/dev/null; }

# stop_pid <pidfile> <name>
stop_pid() {
  if pid_running "$1"; then
    local pid; pid="$(cat "$1")"
    log "Stopping $2 (pid ${pid})"
    kill "${pid}" 2>/dev/null || true
    for _ in $(seq 1 20); do kill -0 "${pid}" 2>/dev/null || break; sleep 0.5; done
    kill -9 "${pid}" 2>/dev/null || true
  fi
  rm -f "$1"
}

relay_running() { pid_running "${RELAY_PID}"; }

relay_ready() {
  [[ "$(curl -s -o /dev/null -w '%{http_code}' "${HTTP_URL}/_readiness" || true)" == "200" ]]
}

port_in_use() { ss -ltnH "sport = :$1" 2>/dev/null | grep -q .; }

deps_up() {
  mkdir -p "${STATE}/data/postgres" "${STATE}/data/minio" "${STATE}/git-repos"
  log "Starting deps (compose project ${PROJECT})..."
  compose up -d --quiet-pull >&2
  log "Waiting for postgres + redis..."
  local pg redis
  for _ in $(seq 1 60); do
    pg="$(docker inspect -f '{{.State.Health.Status}}' buzz-gpui-test-postgres 2>/dev/null || true)"
    redis="$(docker inspect -f '{{.State.Health.Status}}' buzz-gpui-test-redis 2>/dev/null || true)"
    [[ "${pg}" == healthy && "${redis}" == healthy ]] && break
    sleep 1
  done
  [[ "${pg}" == healthy && "${redis}" == healthy ]] || die "deps not healthy (pg=${pg} redis=${redis})"
  for _ in $(seq 1 60); do
    [[ "$(docker inspect -f '{{.State.Status}}' buzz-gpui-test-minio-init 2>/dev/null)" == exited ]] && break
    sleep 1
  done
}

migrate() {
  log "Running migrations (buzz-admin migrate)..."
  with_relay_env "${BIN}/buzz-admin" migrate >&2
}

relay_start() {
  if relay_running; then
    log "Relay already running (pid $(cat "${RELAY_PID}"))"
    return 0
  fi
  for p in "${RELAY_PORT}" "${HEALTH_PORT}" "${METRICS_PORT}"; do
    port_in_use "${p}" && die "port ${p} already in use by something else; refusing to start"
  done
  log "Starting buzz-relay on ${WS_URL} (log: ${RELAY_LOG})"
  local envs=()
  mapfile -t envs < <(relay_env)
  # Direct exec (no function/subshell wrapper) so $! is the relay itself and
  # no inherited fd keeps the caller's pipes open.
  setsid nohup env -C "${STATE}" "${envs[@]}" "${BIN}/buzz-relay" \
    </dev/null >>"${RELAY_LOG}" 2>&1 &
  echo $! > "${RELAY_PID}"
  for _ in $(seq 1 90); do
    relay_running || { tail -40 "${RELAY_LOG}" >&2; die "relay exited during startup"; }
    relay_ready && { log "Relay ready: ${WS_URL}"; return 0; }
    sleep 1
  done
  die "relay not ready after 90s; see ${RELAY_LOG}"
}

relay_stop() { stop_pid "${RELAY_PID}" relay; }

cmd_up() {
  mkdir -p "${STATE}"
  command -v jq >/dev/null || die "jq required"
  build
  ensure_identities
  ensure_relay_env
  deps_up
  migrate
  relay_start
  cat <<EOF
Relay:      ${WS_URL}  (HTTP ${HTTP_URL})
Identities: ${IDENTITIES}
Next:       scripts/test-relay.sh seed
EOF
}

cmd_down() {
  cmd_down_tls
  relay_stop
  log "Stopping deps (compose project ${PROJECT})..."
  compose down >&2
}

cmd_wipe() {
  cmd_down
  log "Deleting ${STATE}/data, git-repos, seed.json (identities, relay key, TLS CA kept)"
  rm -rf "${STATE}/data" "${STATE}/git-repos" "${SEED_FILE}" \
    "${TLS_DIR}/git-repos" "${TLS_STATE_FILE}"
}

cmd_status() {
  echo "== deps (${PROJECT})"
  compose ps -a --format 'table {{.Name}}\t{{.State}}\t{{.Status}}\t{{.Ports}}' || true
  echo "== relay"
  if relay_running; then
    echo "pid $(cat "${RELAY_PID}") running; readiness: $(relay_ready && echo ready || echo NOT ready)"
    echo "NIP-11:"
    curl -s -H 'Accept: application/nostr+json' "${HTTP_URL}/" | jq -c '{name, pubkey, supported_nips, software, version}' 2>/dev/null || true
  else
    echo "not running"
  fi
  [[ -f "${SEED_FILE}" ]] && { echo "== seed (${SEED_FILE})"; jq -c '.channels' "${SEED_FILE}"; }
  echo "== TLS relay (${TLS_WS_URL})"
  if pid_running "${TLS_RELAY_PID}"; then
    echo "relay pid $(cat "${TLS_RELAY_PID}") running; proxy: $(pid_running "${TLS_PROXY_PID}" \
      && echo "pid $(cat "${TLS_PROXY_PID}")" || echo NOT running); readiness over TLS: \
$(tls_ready && echo ready || echo NOT ready)"
    [[ -f "${TLS_STATE_FILE}" ]] && jq -c '{channels, ca_bundle}' "${TLS_STATE_FILE}"
  else
    echo "not running (scripts/test-relay.sh up-tls)"
  fi
  return 0
}

cmd_logs() {
  if [[ "${1:-}" == deps ]]; then compose logs -f --tail 100; else tail -n 200 -F "${RELAY_LOG}"; fi
}

# ── seed + verify ────────────────────────────────────────────────────────────

# Run the repo's own CLI (`buzz`, HTTP + NIP-98) as a named identity, against
# ${CLI_URL} (the plain relay unless the TLS commands switch it).
CLI_URL="${HTTP_URL}"
buzz_as() {
  local who="$1"; shift
  BUZZ_RELAY_URL="${CLI_URL}" BUZZ_PRIVATE_KEY="$(ident "${who}" sk_hex)" "${BIN}/buzz" "$@"
}

require_ready() {
  relay_running && relay_ready || die "relay not running; run: scripts/test-relay.sh up"
  [[ -f "${IDENTITIES}" ]] || die "missing ${IDENTITIES}"
}

# Ensure a channel named $2 exists (created by alice). Echoes its UUID.
ensure_channel() {
  local name="$1" type="$2" vis="$3" desc="$4" id
  id="$(buzz_as alice channels list | jq -r --arg n "${name}" '[.[] | select(.name == $n)][0].channel_id // empty')"
  if [[ -z "${id}" ]]; then
    id="$(buzz_as alice channels create --name "${name}" --type "${type}" --visibility "${vis}" \
      --description "${desc}" | jq -r '.channel_id')"
    log "  created #${name} (${type}/${vis}) ${id}"
  else
    log "  #${name} exists ${id}"
  fi
  [[ "${id}" =~ ^[0-9a-f-]{36}$ ]] || die "could not create/find channel ${name}"
  echo "${id}"
}

# post <who> <channel> <content> [reply-to-event-id] [kind] -> echoes event id
post() {
  local who="$1" ch="$2" content="$3" reply="${4:-}" kind="${5:-}" out id args=()
  [[ -n "${reply}" ]] && args+=(--reply-to "${reply}")
  [[ -n "${kind}" ]] && args+=(--kind "${kind}")
  out="$(buzz_as "${who}" messages send --channel "${ch}" --content "${content}" "${args[@]}")"
  id="$(jq -r '.event_id // empty' <<<"${out}")"
  [[ "${id}" =~ ^[0-9a-f]{64}$ ]] || die "post by ${who} failed: ${out}"
  echo "${id}"
}

react() { buzz_as "$1" reactions add --event "$2" --emoji "$3" >/dev/null || die "reaction by $1 failed"; }

cmd_seed() {
  require_ready
  if [[ -f "${SEED_FILE}" && "${1:-}" != --force ]]; then
    log "Already seeded (${SEED_FILE}); use 'wipe' + 'up' + 'seed' to start over, or 'seed --force' to post again."
    jq . "${SEED_FILE}"
    return 0
  fi
  local alice bob carol
  alice="$(ident alice pk_hex)"; bob="$(ident bob pk_hex)"; carol="$(ident carol pk_hex)"

  log "Relay membership (NIP-43): alice is owner via RELAY_OWNER_PUBKEY; admitting bob + carol"
  for who in bob carol; do
    with_relay_env "${BIN}/buzz-admin" add-member --pubkey "$(ident "${who}" pk_hex)" --role member >&2
    sleep 1.1   # buzz-admin: serialize 13534 roster snapshots (1s granularity)
  done

  log "Profiles (kind 0)"
  for who in alice bob carol; do
    buzz_as "${who}" users set-profile --name "$(ident "${who}" display_name)" \
      --about "Test identity '${who}' ($(ident "${who}" role)) for buzz-gpui" >/dev/null
  done

  log "Channels (NIP-29 kind 9007, created by alice)"
  local general random core forum dm
  general="$(ensure_channel general stream open 'General chat for everyone')"
  random="$(ensure_channel random stream open 'Off-topic')"
  core="$(ensure_channel core-team stream private 'Private: alice + bob only')"
  forum="$(ensure_channel announcements forum open 'Forum-style announcements')"

  log "Channel membership"
  buzz_as bob channels join --channel "${general}" >/dev/null
  buzz_as carol channels join --channel "${general}" >/dev/null
  buzz_as bob channels join --channel "${random}" >/dev/null
  buzz_as alice channels add-member --channel "${core}" --pubkey "${bob}" --role member >/dev/null

  log "Messages, thread replies, reactions"
  local welcome bob_hi carol_hi r1 r2 rnd core1 core2 ann ann_c dm1 dm2
  welcome="$(post alice "${general}" 'Welcome to the buzz-gpui test community! Reply in thread to try threads.')"
  bob_hi="$(post bob "${general}" 'Hi all, Bob here. Testing **markdown** and `code`.')"
  carol_hi="$(post carol "${general}" "Hey @Alice and @Bob, Carol checking in.")"
  r1="$(post bob "${general}" 'Thread reply #1: thanks for setting this up!' "${welcome}")"
  r2="$(post carol "${general}" 'Thread reply #2: threads work.' "${welcome}")"
  react bob "${welcome}" '👍'
  react carol "${welcome}" '🎉'
  react alice "${bob_hi}" '❤️'
  rnd="$(post bob "${random}" 'Random channel: only Bob has joined, but it is open.')"
  core1="$(post alice "${core}" 'Private channel: Carol must not see this.')"
  core2="$(post bob "${core}" 'Confirmed, private reply.' "${core1}")"
  # Forum channels use kind 45001 posts and kind 45003 comments.
  ann="$(post alice "${forum}" 'v0 announcement: forum post (kind 45001).' '' 45001)"
  ann_c="$(post carol "${forum}" 'Forum comment from Carol (kind 45003).' "${ann}" 45003)"

  log "Direct message (alice <-> bob)"
  dm="$(buzz_as alice dms open --pubkey "${bob}" | jq -r '.dm_id // .channel_id // empty')"
  [[ "${dm}" =~ ^[0-9a-f-]{36}$ ]] || die "dms open failed"
  dm1="$(post alice "${dm}" 'Hey Bob, this is a DM.')"
  dm2="$(post bob "${dm}" 'Hi Alice, DM reply.')"

  jq -n --arg general "${general}" --arg random "${random}" --arg core "${core}" \
    --arg forum "${forum}" --arg dm "${dm}" \
    --arg welcome "${welcome}" --arg bob_hi "${bob_hi}" --arg carol_hi "${carol_hi}" \
    --arg r1 "${r1}" --arg r2 "${r2}" --arg rnd "${rnd}" --arg core1 "${core1}" \
    --arg core2 "${core2}" --arg ann "${ann}" --arg ann_c "${ann_c}" --arg dm1 "${dm1}" --arg dm2 "${dm2}" '{
      channels: {general: $general, random: $random, "core-team": $core,
                 announcements: $forum, "dm-alice-bob": $dm},
      events: {welcome: $welcome, bob_hi: $bob_hi, carol_hi: $carol_hi,
               thread_reply_bob: $r1, thread_reply_carol: $r2, random_bob: $rnd,
               core_alice: $core1, core_reply_bob: $core2,
               forum_post: $ann, forum_comment_carol: $ann_c,
               dm_alice: $dm1, dm_bob: $dm2}}' > "${SEED_FILE}"
  log "Seeded -> ${SEED_FILE}"
  jq . "${SEED_FILE}"
}

# ── verify ───────────────────────────────────────────────────────────────────

# One-shot REQ over WS. --fpa waits for the relay's AUTH challenge before
# sending the REQ (without it nak occasionally races the challenge and hangs).
# Call `nak --version` in the parent shell first so ${NAK} is resolved.
ws_req() {
  local fpa=()
  [[ " $* " == *" --auth "* ]] && fpa=(--fpa)
  timeout 20 "${NAK}" req -q "${fpa[@]}" "$@" </dev/null || true  # non-zero on denied auth
}

check() {  # check <description> <expected> <actual>
  if [[ "$2" == "$3" ]]; then printf '  ok    %s (%s)\n' "$1" "$3"
  else printf '  FAIL  %s: expected %s, got %s\n' "$1" "$2" "$3"; FAILED=1; fi
}

cmd_verify() {
  require_ready
  [[ -f "${SEED_FILE}" ]] || die "run seed first"
  FAILED=0
  nak --version >/dev/null
  local general core n code body
  general="$(jq -r '.channels.general' "${SEED_FILE}")"
  core="$(jq -r '.channels["core-team"]' "${SEED_FILE}")"

  echo "NIP-11"
  check "auth_required advertised" true \
    "$(curl -s -H 'Accept: application/nostr+json' "${HTTP_URL}/" | jq -r '.limitation.auth_required')"

  echo "WebSocket REQ with NIP-42 AUTH (nak)"
  n="$(ws_req --auth --sec "$(ident bob sk_hex)" -k 9 -t "h=${general}" -l 100 "${WS_URL}" 2>/dev/null | wc -l)"
  check "bob reads #general kind 9" 5 "${n}"
  # Reactions are stored channel-scoped but carry no h tag, so nak's local
  # filter check drops them for a #h REQ even though the relay sends them.
  n="$(ws_req --auth --sec "$(ident bob sk_hex)" -k 7 -e "$(jq -r .events.welcome "${SEED_FILE}")" \
        -l 100 "${WS_URL}" 2>/dev/null | wc -l)"
  check "bob reads reactions on the welcome message (kind 7, #e)" 2 "${n}"
  n="$(ws_req --auth --sec "$(ident bob sk_hex)" -k 9 -t "h=${core}" -l 100 "${WS_URL}" 2>/dev/null | wc -l)"
  check "bob reads private #core-team" 2 "${n}"
  n="$(ws_req --auth --sec "$(ident carol sk_hex)" -k 9 -t "h=${core}" -l 100 "${WS_URL}" 2>/dev/null | wc -l)"
  check "carol (non-member) reads private #core-team" 0 "${n}"
  n="$(ws_req -k 9 -t "h=${general}" -l 100 "${WS_URL}" 2>/dev/null | wc -l)"
  check "unauthenticated REQ gets nothing" 0 "${n}"
  n="$(ws_req --auth --sec "$(nak key generate)" -k 9 -t "h=${general}" -l 100 "${WS_URL}" 2>/dev/null | wc -l)"
  check "authenticated non-member gets nothing" 0 "${n}"

  echo "HTTP POST /query with NIP-98"
  n="$(buzz_as bob messages get --channel "${general}" --limit 100 | jq 'if type == "array" then length else (.messages // .events | length) end')"
  check "buzz-cli 'messages get' (POST /query, NIP-98) as bob" 5 "${n}"
  body="$(jq -nc --arg h "${general}" '[{kinds: [9], "#h": [$h], limit: 100}]')"
  # nak curl ignores --sec; it signs with NOSTR_SECRET_KEY.
  n="$(NOSTR_SECRET_KEY="$(ident carol sk_hex)" nak curl -s -X POST </dev/null -H 'Content-Type: application/json' \
        -d "${body}" "${HTTP_URL}/query" | jq 'length')"
  check "raw POST /query via 'nak curl' (NIP-98) as carol" 5 "${n}"
  # nak's NIP-98 event has no nonce/payload tag, so a second identical request
  # from the same key in the same second is rejected as a replay; use bob.
  body="$(jq -nc --arg h "${general}" '[{kinds: [7], "#h": [$h], limit: 100}]')"
  n="$(NOSTR_SECRET_KEY="$(ident bob sk_hex)" nak curl -s -X POST </dev/null -H 'Content-Type: application/json' \
        -d "${body}" "${HTTP_URL}/query" | jq 'length')"
  check "raw POST /query #general reactions (kind 7, #h) as bob" 3 "${n}"
  code="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d "${body}" "${HTTP_URL}/query")"
  check "POST /query without auth" 401 "${code}"

  [[ "${FAILED}" == 0 ]] && echo "All checks passed." || { echo "Some checks FAILED."; return 1; }
}

# ── TLS relay ────────────────────────────────────────────────────────────────

SOCAT=""
socat_bin() {
  if [[ -z "${SOCAT}" ]]; then
    if type -P socat >/dev/null 2>&1; then
      SOCAT="$(type -P socat)"
    else
      SOCAT="$(nix build nixpkgs#socat --no-link --print-out-paths 2>/dev/null)/bin/socat"
    fi
    [[ -x "${SOCAT}" ]] || die "could not get socat (nix build nixpkgs#socat failed)"
  fi
  echo "${SOCAT}"
}

# Upstream buzz-acp's WebSocket client trusts only the bundled webpki roots,
# so it could never trust the test CA. Building it with tokio-tungstenite's
# native-roots feature adds the roots from SSL_CERT_FILE (the controller
# passes that variable through to buzz-acp). No source change.
build_acp() {
  ensure_src
  log "Building buzz-acp (with native TLS roots)..."
  (cd "${SRC}" && nix shell nixpkgs#cmake nixpkgs#pkg-config nixpkgs#protobuf --command \
    cargo build -q -p buzz-acp --features tokio-tungstenite/rustls-tls-native-roots) \
    >>"${STATE}/build.log" 2>&1 || die "cargo build buzz-acp failed; see ${STATE}/build.log"
}

# A local CA and a localhost certificate it signs. Generated once.
tls_certs() {
  mkdir -p "${TLS_DIR}"
  if [[ ! -f "${TLS_DIR}/server.pem" ]]; then
    log "Generating test CA and localhost certificate -> ${TLS_DIR}"
    (
      umask 077
      cd "${TLS_DIR}"
      openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
        -keyout ca.key -out ca.pem -days 3650 -subj "/CN=buzz-gpui test relay CA" \
        -addext "basicConstraints=critical,CA:TRUE" \
        -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
      openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
        -keyout server.key -out server.csr -subj "/CN=localhost" 2>/dev/null
      printf '%s\n' "basicConstraints=critical,CA:FALSE" \
        "keyUsage=critical,digitalSignature" "extendedKeyUsage=serverAuth" \
        "subjectAltName=DNS:localhost,IP:127.0.0.1" > server.ext
      openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key \
        -set_serial "0x$(openssl rand -hex 16)" -days 825 -extfile server.ext \
        -out server.pem 2>/dev/null
      rm -f server.csr server.ext
      chmod 644 ca.pem server.pem
    ) || die "certificate generation failed"
  fi
  # SSL_CERT_FILE replaces the system store (rustls-native-certs, OpenSSL),
  # so the bundle carries the system roots too.
  local system="${NIX_SSL_CERT_FILE:-/etc/ssl/certs/ca-certificates.crt}"
  { [[ -f "${system}" ]] && cat "${system}"; cat "${TLS_CA}"; } > "${TLS_BUNDLE}"
}

tls_db() {
  local exists
  exists="$(docker exec buzz-gpui-test-postgres psql -U buzz -d buzz -tAc \
    "SELECT 1 FROM pg_database WHERE datname = '${TLS_DB}'")"
  if [[ -z "${exists}" ]]; then
    log "Creating database ${TLS_DB}"
    docker exec buzz-gpui-test-postgres createdb -U buzz "${TLS_DB}" >&2
  fi
}

tls_relay_env() {
  relay_env | grep -vE '^(DATABASE_URL|REDIS_URL|RELAY_URL|BUZZ_BIND_ADDR|BUZZ_HEALTH_PORT|BUZZ_METRICS_PORT|BUZZ_GIT_REPO_PATH)='
  cat <<EOF
DATABASE_URL=postgres://buzz:buzz_dev@127.0.0.1:${PG_PORT}/${TLS_DB}
REDIS_URL=${REDIS_URL}/1
RELAY_URL=${TLS_WS_URL}
BUZZ_BIND_ADDR=127.0.0.1:${TLS_BACKEND_PORT}
BUZZ_HEALTH_PORT=${TLS_HEALTH_PORT}
BUZZ_METRICS_PORT=${TLS_METRICS_PORT}
BUZZ_GIT_REPO_PATH=${TLS_DIR}/git-repos
BUZZ_ALLOW_NIP_OA_AUTH=true
EOF
}

tls_ready() {
  [[ "$(curl -s --cacert "${TLS_CA}" -o /dev/null -w '%{http_code}' \
    "${TLS_HTTP_URL}/_readiness" || true)" == "200" ]]
}

tls_proxy_start() {
  pid_running "${TLS_PROXY_PID}" && return 0
  port_in_use "${TLS_PORT}" && die "port ${TLS_PORT} already in use by something else; refusing to start"
  local socat; socat="$(socat_bin)"
  log "Starting TLS proxy 127.0.0.1:${TLS_PORT} -> 127.0.0.1:${TLS_BACKEND_PORT} (log: ${TLS_PROXY_LOG})"
  # Plain TCP relay after the handshake, so Host and WebSocket upgrades pass
  # through untouched.
  setsid nohup "${socat}" \
    "OPENSSL-LISTEN:${TLS_PORT},bind=127.0.0.1,reuseaddr,fork,cert=${TLS_DIR}/server.pem,key=${TLS_DIR}/server.key,verify=0" \
    "TCP:127.0.0.1:${TLS_BACKEND_PORT}" </dev/null >>"${TLS_PROXY_LOG}" 2>&1 &
  echo $! > "${TLS_PROXY_PID}"
}

tls_relay_start() {
  if pid_running "${TLS_RELAY_PID}"; then
    log "TLS relay already running (pid $(cat "${TLS_RELAY_PID}"))"
  else
    for p in "${TLS_BACKEND_PORT}" "${TLS_HEALTH_PORT}" "${TLS_METRICS_PORT}"; do
      port_in_use "${p}" && die "port ${p} already in use by something else; refusing to start"
    done
    mkdir -p "${TLS_DIR}/git-repos"
    log "Starting TLS buzz-relay for ${TLS_WS_URL} (log: ${TLS_RELAY_LOG})"
    local envs=()
    mapfile -t envs < <(tls_relay_env)
    setsid nohup env -C "${TLS_DIR}" "${envs[@]}" "${BIN}/buzz-relay" \
      </dev/null >>"${TLS_RELAY_LOG}" 2>&1 &
    echo $! > "${TLS_RELAY_PID}"
  fi
  for _ in $(seq 1 90); do
    pid_running "${TLS_RELAY_PID}" || { tail -40 "${TLS_RELAY_LOG}" >&2; die "TLS relay exited during startup"; }
    pid_running "${TLS_PROXY_PID}" || { tail -20 "${TLS_PROXY_LOG}" >&2; die "TLS proxy exited"; }
    tls_ready && { log "TLS relay ready: ${TLS_WS_URL}"; return 0; }
    sleep 1
  done
  die "TLS relay not ready after 90s; see ${TLS_RELAY_LOG}"
}

# alice owns the TLS community (RELAY_OWNER_PUBKEY); it gets an open
# #general. Agents are admitted through their owner's NIP-OA attestation
# (BUZZ_ALLOW_NIP_OA_AUTH), not as relay members.
tls_seed() {
  local general
  CLI_URL="${TLS_HTTP_URL}"
  general="$(SSL_CERT_FILE="${TLS_BUNDLE}" ensure_channel general stream open 'General chat for everyone')"
  CLI_URL="${HTTP_URL}"
  jq -n --arg ws "${TLS_WS_URL}" --arg http "${TLS_HTTP_URL}" --arg host "${TLS_HOST}" \
    --arg ca "${TLS_CA}" --arg bundle "${TLS_BUNDLE}" \
    --arg acp "${BIN}/buzz-acp" --arg cli "${BIN}/buzz" --arg general "${general}" \
    '{relay_ws_url: $ws, relay_http_url: $http, community_host: $host,
      ca_file: $ca, ca_bundle: $bundle, buzz_acp: $acp, buzz_cli: $cli,
      channels: {general: $general}}' > "${TLS_STATE_FILE}"
}

cmd_up_tls() {
  mkdir -p "${STATE}"
  command -v jq >/dev/null || die "jq required"
  command -v openssl >/dev/null || die "openssl required"
  build
  build_acp
  ensure_identities
  ensure_relay_env
  deps_up
  tls_certs
  tls_db
  log "Running migrations on ${TLS_DB}..."
  with_env_of tls_relay_env "${BIN}/buzz-admin" migrate >&2
  tls_proxy_start
  tls_relay_start
  tls_seed
  cat <<EOF
TLS relay:  ${TLS_WS_URL}  (HTTP ${TLS_HTTP_URL})
CA:         ${TLS_CA}
Trust:      SSL_CERT_FILE=${TLS_BUNDLE}
State:      ${TLS_STATE_FILE}
EOF
}

cmd_down_tls() {
  stop_pid "${TLS_PROXY_PID}" "TLS proxy"
  stop_pid "${TLS_RELAY_PID}" "TLS relay"
}

case "${1:-}" in
  up) cmd_up ;;
  down) cmd_down ;;
  restart) relay_stop; relay_start ;;
  wipe) cmd_wipe ;;
  status) cmd_status ;;
  logs) shift; cmd_logs "$@" ;;
  seed) shift; cmd_seed "$@" ;;
  verify) cmd_verify ;;
  build) build ;;
  up-tls) cmd_up_tls ;;
  down-tls) cmd_down_tls ;;
  *) sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
