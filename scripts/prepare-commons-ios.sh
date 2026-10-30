#!/bin/bash
# Configure the independent Voltage client without changing upstream defaults.
set -euo pipefail
repo_root=$(cd "$(dirname "$0")/.." && pwd)
destination="$repo_root/mobile/ios/Flutter/AppOverrides.xcconfig"
if [[ -e "$destination" ]]; then
  echo "AppOverrides.xcconfig already exists. Review it before replacing your local signing configuration." >&2
  exit 1
fi
team=${VOLTAGE_APPLE_TEAM_ID:-}
if [[ -n "$team" && ! "$team" =~ ^[A-Z0-9]{10}$ ]]; then
  echo 'VOLTAGE_APPLE_TEAM_ID must be a ten-character Apple team ID.' >&2
  exit 1
fi
temporary=$(mktemp "$destination.XXXXXX")
trap 'rm -f "$temporary"' EXIT
cat "$repo_root/voltage/mobile/Commons.xcconfig" > "$temporary"
if [[ -n "$team" ]]; then
  printf '\nBUZZ_DEVELOPMENT_TEAM = %s\n' "$team" >> "$temporary"
fi
mv "$temporary" "$destination"
echo 'Configured Voltage Commons (cloud.voltage.workspace).'
if [[ -z "$team" ]]; then
  echo 'Signing team is unset. Unsigned builds are available; device distribution requires a Voltage Apple team.'
fi
