#!/usr/bin/env bash
# Capture real-app reference screenshots (light + dark) using buzz-app's own
# Playwright synthetic-relay fixture. Requires `pnpm install` in buzz-app.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
export BUZZ_APP="${BUZZ_APP:-$HOME/projects/buzz-app}"
export CHROMIUM="${CHROMIUM:-$(nix build nixpkgs#chromium --no-link --print-out-paths)/bin/chromium}"
cd "$BUZZ_APP"
exec node node_modules/@playwright/test/cli.js test --config "$here/playwright.config.mjs" "$@"
