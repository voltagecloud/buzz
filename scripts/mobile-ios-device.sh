#!/bin/bash
# Use the checkout's pinned Flutter and saved fork identity on a named device.
set -euo pipefail

if [[ $# -ne 1 || -z "$1" || "$1" == -* ]]; then
    echo "Usage: scripts/mobile-ios-device.sh PHONE_ID (from flutter devices)" >&2
    exit 2
fi
if [[ "$(uname -s)" != Darwin ]]; then
    echo "Installing on an iPhone requires a Mac with Xcode. See mobile/IOS_FORK.md." >&2
    exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
config="$repo_root/mobile/ios/Flutter/AppOverrides.xcconfig"
if [[ ! -f "$config" ]] || \
    ! grep -Eq '^[[:space:]]*BUNDLE_IDENTIFIER[[:space:]]*=[[:space:]]*[^[:space:]]+' "$config" || \
    ! grep -Eq '^[[:space:]]*BUZZ_DEVELOPMENT_TEAM[[:space:]]*=[[:space:]]*[A-Z0-9]{10}[[:space:]]*$' "$config"; then
    echo "Configure your fork first: just mobile-ios-setup BUNDLE_ID TEAM_ID" >&2
    exit 1
fi
if ! xcodebuild -version >/dev/null 2>&1; then
    echo "Select a full Xcode installation with xcode-select, and complete Xcode's first-run setup." >&2
    exit 1
fi
if ! command -v pod >/dev/null 2>&1; then
    echo "Install CocoaPods on your Mac before running this command. See mobile/IOS_FORK.md." >&2
    exit 1
fi

cd "$repo_root"
unset GIT_DIR GIT_WORK_TREE
# shellcheck source=../bin/activate-hermit
. ./bin/activate-hermit
./scripts/mobile-worktree-overrides.sh
cd mobile
flutter pub get

# Match mobile-dev: an explicit origin wins, otherwise use the Xcode override.
# Never evaluate xcconfig as shell code; its https:/$()/ spelling is literal.
gateway_url="${BUZZ_PUSH_GATEWAY_URL:-}"
if [[ -z "$gateway_url" ]]; then
    gateway_url="$(sed -nE 's/^[[:space:]]*BUZZ_PUSH_GATEWAY_URL[[:space:]]*=[[:space:]]*(.*[^[:space:]])[[:space:]]*$/\1/p' "$config" | tail -n 1 | sed 's/\$()//g')"
fi
set -- run --debug --no-pub --device-id "$1"
if [[ -n "$gateway_url" ]]; then
    set -- "$@" "--dart-define=BUZZ_PUSH_GATEWAY_URL=$gateway_url"
fi
exec flutter "$@"
