#!/bin/bash
# Configure a fork's iPhone identity without editing tracked Xcode files.
set -euo pipefail

usage() {
    cat <<'USAGE'
Usage: scripts/mobile-ios-setup.sh BUNDLE_ID TEAM_ID [DISPLAY_NAME]

Example: scripts/mobile-ios-setup.sh io.voltage.buzz.mobile ABCDE12345 "Buzz Voltage"

Writes the gitignored mobile/ios/Flutter/AppOverrides.xcconfig. Other settings,
including a configured push gateway, are preserved. Use the same bundle ID and
team for subsequent updates. This does not register Apple IDs or signing profiles.
USAGE
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -lt 2 || $# -gt 3 ]]; then
    usage >&2
    exit 2
fi

bundle_id="$1"
team_id="$2"
display_name="${3-Buzz Voltage}"
if [[ ! "$bundle_id" =~ ^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+$ || ${#bundle_id} -gt 235 ]]; then
    echo "Use a reverse-domain bundle ID, with room for .NotificationService (at most 235 characters)." >&2
    exit 2
fi
if [[ "$bundle_id" == "xyz.block.buzz.mobile" || "$bundle_id" == "xyz.block.buzz.dogfood.mobile" ]]; then
    echo "Choose a bundle ID owned by your Apple team, rather than the upstream Buzz IDs." >&2
    exit 2
fi
if [[ ! "$team_id" =~ ^[A-Z0-9]{10}$ ]]; then
    echo "Apple Team ID must contain exactly 10 uppercase letters or digits." >&2
    exit 2
fi
if [[ -z "$display_name" || ${#display_name} -gt 64 || ! "$display_name" =~ ^[A-Za-z0-9._\(\)\ -]+$ ]]; then
    echo "Display name must contain 1–64 letters, numbers, spaces, periods, underscores, parentheses, or hyphens." >&2
    exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
config_dir="$repo_root/mobile/ios/Flutter"
config="$config_dir/AppOverrides.xcconfig"
if [[ ! -d "$config_dir" || -L "$config" || ( -e "$config" && ! -f "$config" ) ]]; then
    echo "Run from a Buzz checkout with a regular AppOverrides.xcconfig file, or no existing override file." >&2
    exit 1
fi

existing=/dev/null
[[ ! -f "$config" ]] || existing="$config"
temporary="$(mktemp "$config_dir/.AppOverrides.xcconfig.XXXXXX")"
trap 'rm -f "$temporary"' EXIT

# Filter only the four managed settings. Keep comments, gateway definitions,
# capability environments, and other operator settings byte-for-byte per line.
awk '!/^[[:space:]]*(BUNDLE_IDENTIFIER|BUZZ_DEVELOPMENT_TEAM|APP_DISPLAY_NAME|CODE_SIGN_STYLE)[[:space:]]*=/' \
    "$existing" > "$temporary"
cat >> "$temporary" <<XCCONFIG
BUNDLE_IDENTIFIER = $bundle_id
BUZZ_DEVELOPMENT_TEAM = $team_id
APP_DISPLAY_NAME = $display_name
CODE_SIGN_STYLE = Automatic
XCCONFIG

if [[ -f "$config" ]] && cmp -s "$temporary" "$config"; then
    echo "iPhone identity already configured."
else
    mv "$temporary" "$config"
    echo "Configured mobile/ios/Flutter/AppOverrides.xcconfig (gitignored)."
fi
printf 'App: %s\nTeam: %s\nExtension: %s.NotificationService\nApp Group: group.%s\n' \
    "$bundle_id" "$team_id" "$bundle_id" "$bundle_id"
echo "Next: follow mobile/IOS_FORK.md for Apple capabilities, then run just mobile-ios-device PHONE_ID on your Mac."
