#!/usr/bin/env bash
# Build the package's app, and Voltage's provider beside it when requested.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
repo="$(cd "$here/../.." && pwd)"
cd "$here"
target="${CARGO_TARGET_DIR:-$here/target}"
features=()
if [ "${BUZZ_GPUI_VOLTAGE:-0}" = 1 ]; then
  features=(--features voltage-sandbox)
  cargo build --manifest-path "$repo/Cargo.toml" --locked --release \
    -p buzz-backend-relay --target-dir "$target/voltage-provider"
fi
cargo build --locked --release -p buzz-gpui "${features[@]}" "$@"
if [ "${BUZZ_GPUI_VOLTAGE:-0}" = 1 ]; then
  suffix=""
  if [ "${OS:-}" = Windows_NT ]; then suffix=.exe; fi
  cp "$target/voltage-provider/release/buzz-backend-relay$suffix" "$target/release/"
fi
