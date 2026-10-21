#!/usr/bin/env bash
# Re-capture every buzz-app reference screenshot into docs/port/screenshots.
#
#   scripts/reference/capture-all.sh            # everything
#   JOBS=designViewer scripts/reference/capture-all.sh fixtures   # one fixture job
#   FILTER='^design-button' JOBS=designViewer ... fixtures        # name filter
#   scripts/reference/capture-all.sh app        # only the real-app Playwright run
#
# Needs: node >= 24 and pnpm on PATH (NixOS system packages work; Hermit's
# bin/ is not required), nix (for chromium), python3 (static file server).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
export BUZZ_APP="${BUZZ_APP:-$HOME/projects/buzz-app}"
export SHOT_DIR="${SHOT_DIR:-$(cd "$here/../.." && pwd)/docs/port/screenshots}"
export CHROMIUM="${CHROMIUM:-$(nix build nixpkgs#chromium --no-link --print-out-paths)/bin/chromium}"
export WEB="http://localhost:1531" DESIGN="http://localhost:1542" FIXTURES="http://127.0.0.1:1561"
export FIXTURE_OUT="${FIXTURE_OUT:-/tmp/buzz-fixtures-dist}"
what="${1:-all}"
mkdir -p "$SHOT_DIR"

(cd "$BUZZ_APP" && pnpm install --frozen-lockfile >/dev/null)

pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  # pnpm leaves vite children behind; stop anything still bound to our ports.
  for port in 1531 1542 1561; do
    for pid in $(ss -ltnpH "sport = :$port" 2>/dev/null | grep -o 'pid=[0-9]*' | cut -d= -f2); do
      kill "$pid" 2>/dev/null || true
    done
  done
}
trap cleanup EXIT

if [ "$what" = all ] || [ "$what" = fixtures ]; then
  # Shell-mode web app (no BUZZ_DEV_VIEWER => fixtures) and design viewer.
  (cd "$BUZZ_APP" && BUZZ_DEV_VIEWER= exec pnpm exec vite --port 1531 --strictPort >/tmp/buzz-web.log 2>&1) &
  pids+=($!)
  (cd "$BUZZ_APP" && exec pnpm exec vite --config vite.design.config.ts --port 1542 --strictPort --open false >/tmp/buzz-design.log 2>&1) &
  pids+=($!)
  # Production build of fixtures whose dev-mode CSS order differs from the app.
  (cd "$BUZZ_APP" && node node_modules/vite/bin/vite.js build --config "$here/vite.fixtures.config.mjs" >/tmp/buzz-fixtures-build.log 2>&1)
  (cd "$FIXTURE_OUT" && exec python3 -m http.server 1561 --bind 127.0.0.1 >/tmp/buzz-fixtures-http.log 2>&1) &
  pids+=($!)
  for url in "$WEB/" "$DESIGN/tests/fixtures/design-system.html" "$FIXTURES/"; do
    for _ in $(seq 60); do curl -sf -o /dev/null "$url" && break; sleep 1; done
  done
  node "$here/capture-fixtures.mjs" "${FILTER:-}"
  node "$here/dump-tokens.mjs" "$(dirname "$SHOT_DIR")/tokens-resolved.json"
fi

if [ "$what" = all ] || [ "$what" = app ]; then
  "$here/capture-app.sh"
fi
