#!/usr/bin/env bash
set -euo pipefail
image=${1:?usage: test-image.sh IMAGE}
scratch=$(mktemp -d)
container=
cleanup() {
  if [[ -n "$container" ]]; then docker rm -f "$container" >/dev/null; fi
  rm -rf "$scratch"
}
trap cleanup EXIT
chmod 755 "$scratch"
cat > "$scratch/config.json" <<'JSON'
{"relay_url":"https://127.0.0.1:9","github_repos":{"example/repo":"0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10"}}
JSON
chmod 444 "$scratch/config.json"
container=$(docker run -d --read-only --cap-drop ALL --security-opt no-new-privileges \
  --user 65532:65532 --memory 128m --cpus 1 \
  -p 127.0.0.1::5010 -v "$scratch/config.json:/etc/buzz-ingest/config.json:ro" \
  -e BUZZ_INGEST_CONFIG=/etc/buzz-ingest/config.json \
  -e BUZZ_INGEST_PRIVATE_KEY=1111111111111111111111111111111111111111111111111111111111111111 \
  -e BUZZ_INGEST_GITHUB_SECRET=synthetic-image-test "$image")
address=$(docker port "$container" 5010/tcp)
for attempt in {1..40}; do
  if curl -fsS "http://$address/health" >/dev/null; then break; fi
  sleep 0.2
done
[[ $(curl -s -o /dev/null -w '%{http_code}' "http://$address/health") = 200 ]]
[[ $(curl -s -o /dev/null -w '%{http_code}' "http://$address/ready") = 503 ]]
[[ $(curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' \
  -d '{}' "http://$address/ingest/github") = 401 ]]
echo 'buzz-ingest image: hardened startup, health, readiness and signature refusal passed'
