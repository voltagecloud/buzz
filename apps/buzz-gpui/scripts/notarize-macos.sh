#!/usr/bin/env bash
# Notarizes a .dmg that scripts/package-macos.sh signed with a Developer ID,
# staples the ticket to it, and checks that Gatekeeper accepts the image and
# the app inside as notarized, so it opens on another Mac with no warning.
#
# Authenticates with an App Store Connect API key:
#   APPLE_API_KEY_PATH  the .p8 key file
#   APPLE_API_KEY_ID    its key ID
#   APPLE_API_ISSUER    its issuer ID
#
# Usage: scripts/notarize-macos.sh dist/Buzz-<version>-macos-<arch>.dmg
set -euo pipefail
dmg="$1"
work="$(mktemp -d)"
mount="$work/mount"
cleanup() {
  hdiutil detach "$mount" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

credentials=(--key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER")
# notarytool exits 0 for a rejected submission too; the status says which.
xcrun notarytool submit "$dmg" "${credentials[@]}" --wait --timeout 1h \
  --output-format json >"$work/submission.json"
cat "$work/submission.json"
read -r id status < <(python3 -c '
import json, sys
s = json.load(open(sys.argv[1]))
print(s["id"], s["status"])' "$work/submission.json")
if [ "$status" != Accepted ]; then
  echo "Notarization ended $status. Apple's log:" >&2
  xcrun notarytool log "$id" "${credentials[@]}" >&2 || true
  exit 1
fi

xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
mkdir "$mount"
hdiutil attach -nobrowse -readonly -mountpoint "$mount" "$dmg" >/dev/null
spctl --assess --type execute --verbose=2 "$mount/Buzz.app" 2>&1 | tee "$work/assess"
grep -q 'source=Notarized Developer ID' "$work/assess"
