#!/usr/bin/env bash
# Smoke-test a relay image built from the root Dockerfile (target runtime):
# the binaries load their shared libraries, buzz-admin runs, and the web
# bundles are present. The relay itself needs Postgres, so it does not start.
set -euo pipefail

image=${1:?usage: test-relay-image.sh <image>}

docker run --rm --entrypoint /usr/local/bin/buzz-admin "$image" --help >/dev/null

docker run --rm --entrypoint /bin/sh "$image" -c '
  set -eu
  test "$(id -u)" = 1000
  for binary in buzz-relay buzz-admin buzz-pair-relay; do
    if ldd "/usr/local/bin/$binary" | grep "not found"; then
      echo "$binary is missing shared libraries" >&2
      exit 1
    fi
  done
  test -f /srv/buzz/web/index.html
  test -f /srv/buzz/admin-web/index.html
  git --version >/dev/null
'

echo "relay image $image passed"
