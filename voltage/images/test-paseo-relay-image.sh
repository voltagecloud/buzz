#!/usr/bin/env bash
# Smoke-test a Paseo relay image the way production runs it (voltagecloud/cd
# charts/paseo-relay): non-root, read-only root filesystem, no capabilities,
# no Erlang distribution, and the prod memory and socket limits. The relay
# must answer its probes, upgrade a daemon socket on /ws, and listen on no
# port other than 4000.
set -euo pipefail

image=${1:?usage: test-paseo-relay-image.sh <image>}
name="paseo-relay-smoke-$$"

cleanup() {
  docker rm --force "$name" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run --detach --name "$name" \
  --read-only --user 65532:65532 --cap-drop ALL --security-opt no-new-privileges \
  --memory 1g --cpus 1 \
  --env PASEO_RELAY_HOST=0.0.0.0 \
  --env PASEO_RELAY_PORT=4000 \
  --env RELEASE_DISTRIBUTION=none \
  --env LANG=C.UTF-8 \
  --env ERL_CRASH_DUMP_SECONDS=0 \
  --env PASEO_RELAY_MEMORY_WATERMARK_BYTES=805306368 \
  --env PASEO_RELAY_INGRESS_BUDGET_BYTES=268435456 \
  --env PASEO_RELAY_CONNECTIONS_PER_ACCEPTOR=40 \
  --publish 127.0.0.1::4000 "$image" >/dev/null

base="http://$(docker port "$name" 4000/tcp | head -n 1)"

for _ in $(seq 1 100); do
  if curl --fail --silent "$base/health" >/dev/null 2>&1; then
    break
  fi
  sleep 0.2
done

if ! curl --fail --silent "$base/health" >/dev/null 2>&1; then
  docker logs "$name" >&2
  echo "the relay did not start" >&2
  exit 1
fi

test "$(curl --fail --silent --show-error "$base/health")" = '{"status":"ok"}'
test "$(curl --fail --silent --show-error "$base/ready")" = '{"status":"ready"}'
curl --fail --silent --show-error "$base/metrics" | grep --quiet '^paseo_relay_ready 1$'

# A daemon control socket upgrades. curl then waits on the open socket until
# --max-time, so its exit status is ignored; the status code is what counts.
status=$(curl --silent --output /dev/null --write-out '%{http_code}' --max-time 3 \
  --header 'Connection: Upgrade' \
  --header 'Upgrade: websocket' \
  --header 'Sec-WebSocket-Version: 13' \
  --header 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
  "$base/ws?serverId=smoke&role=server&v=2" || true)
if [ "$status" != 101 ]; then
  echo "expected 101 from /ws, got $status" >&2
  exit 1
fi

# Only 0.0.0.0:4000 (0FA0) listens: no EPMD (4369) or distribution port.
listeners=$(docker exec "$name" sh -c \
  'awk "NR > 1 && \$4 == \"0A\" { print \$2 }" /proc/net/tcp /proc/net/tcp6 2>/dev/null' | sort -u)
if [ "$listeners" != "00000000:0FA0" ]; then
  echo "unexpected TCP listeners: $listeners" >&2
  exit 1
fi

test "$(docker exec "$name" id -u)" = 65532

echo "paseo relay image $image passed"
