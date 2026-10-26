#!/bin/bash
# Exercise the real setup/device commands with isolated configs and host tools.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/scripts" "$fixture/mobile/ios/Flutter" "$fixture/bin"
cp "$repo_root/scripts/mobile-ios-setup.sh" "$repo_root/scripts/mobile-ios-device.sh" "$fixture/scripts/"
config="$fixture/mobile/ios/Flutter/AppOverrides.xcconfig"
setup="$fixture/scripts/mobile-ios-setup.sh"
device="$fixture/scripts/mobile-ios-device.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }

/bin/bash "$setup" io.example.buzz ABCDE12345 "Buzz Example" >/dev/null
grep -qx 'BUNDLE_IDENTIFIER = io.example.buzz' "$config"
grep -qx 'BUZZ_DEVELOPMENT_TEAM = ABCDE12345' "$config"
grep -qx 'APP_DISPLAY_NAME = Buzz Example' "$config"
grep -qx 'CODE_SIGN_STYLE = Automatic' "$config"
echo "PASS fresh fork configuration"

cat >> "$config" <<'CONFIG'
// Operator settings must survive subsequent setup runs.
BUZZ_PUSH_GATEWAY_URL = https:/$()/push.example
BUZZ_APP_ATTEST_ENVIRONMENT = development
CONFIG
/bin/bash "$setup" io.example.updated ZYXWV98765 "Buzz Updated" >/dev/null
grep -Fqx 'BUZZ_PUSH_GATEWAY_URL = https:/$()/push.example' "$config"
grep -qx 'BUZZ_APP_ATTEST_ENVIRONMENT = development' "$config"
[[ "$(grep -c '^BUNDLE_IDENTIFIER =' "$config")" == 1 ]]
grep -qx 'BUNDLE_IDENTIFIER = io.example.updated' "$config"
grep -qx 'BUZZ_DEVELOPMENT_TEAM = ZYXWV98765' "$config"
cp "$config" "$fixture/before"
/bin/bash "$setup" io.example.updated ZYXWV98765 "Buzz Updated" >/dev/null
cmp "$fixture/before" "$config"
echo "PASS existing settings survive and repeated setup is idempotent"

reject() {
    if /bin/bash "$setup" "$@" >/dev/null 2>&1; then
        fail "setup accepted invalid arguments"
    fi
    cmp "$fixture/before" "$config" || fail "invalid input changed the config"
}
reject example ABCDE12345
reject 'io.example.buzz$(touch BAD)' ABCDE12345
reject io.example.buzz short
reject io.example.buzz abcde12345
reject io.example.buzz ABCDE12345 ''
reject io.example.buzz ABCDE12345 'Bad // comment'
reject io.example.buzz ABCDE12345 $'Bad\nCODE_SIGN_STYLE = Manual'
reject xyz.block.buzz.mobile ABCDE12345
reject xyz.block.buzz.dogfood.mobile ABCDE12345
reject io.example.buzz ABCDE12345 Name extra
mv "$config" "$fixture/symlink-target"
ln -s "$fixture/symlink-target" "$config"
reject io.example.buzz ABCDE12345
rm "$config"
mv "$fixture/symlink-target" "$config"
echo "PASS invalid input and symlink rejection leave configuration intact"

# Stub only external tools; run the actual device command and config parser.
cat > "$fixture/bin/uname" <<'STUB'
#!/bin/bash
printf '%s\n' "${TEST_HOST_OS:-Darwin}"
STUB
cat > "$fixture/bin/xcodebuild" <<'STUB'
#!/bin/bash
exit "${TEST_XCODE_EXIT:-0}"
STUB
cat > "$fixture/bin/pod" <<'STUB'
#!/bin/bash
exit 0
STUB
cat > "$fixture/bin/activate-hermit" <<'STUB'
export TEST_HERMIT_ACTIVATED=1
STUB
cat > "$fixture/scripts/mobile-worktree-overrides.sh" <<'STUB'
#!/bin/bash
printf '%s\n' worktree-overrides >> "$CALL_LOG"
STUB
cat > "$fixture/bin/flutter" <<'STUB'
#!/bin/bash
set -eu
[[ "${TEST_HERMIT_ACTIVATED:-}" == 1 ]]
printf '%s\n' flutter "$@" >> "$CALL_LOG"
if [[ "$1" == pub ]]; then exit "${TEST_PUB_EXIT:-0}"; fi
STUB
chmod +x "$fixture/bin/uname" "$fixture/bin/xcodebuild" "$fixture/bin/pod" \
    "$fixture/bin/flutter" "$fixture/scripts/mobile-worktree-overrides.sh"
export PATH="$fixture/bin:$PATH"
export CALL_LOG="$fixture/calls"
unset BUZZ_PUSH_GATEWAY_URL
/bin/bash "$device" 'physical-phone-id' > /dev/null
printf '%s\n' worktree-overrides flutter pub get flutter run --debug --no-pub \
    --device-id physical-phone-id '--dart-define=BUZZ_PUSH_GATEWAY_URL=https://push.example' > "$fixture/expected"
diff -u "$fixture/expected" "$CALL_LOG"
echo "PASS pinned tools, dependency install, device selection, and saved push gateway"

: > "$CALL_LOG"
BUZZ_PUSH_GATEWAY_URL=https://other.example /bin/bash "$device" 'phone with spaces' > /dev/null
printf '%s\n' worktree-overrides flutter pub get flutter run --debug --no-pub \
    --device-id 'phone with spaces' '--dart-define=BUZZ_PUSH_GATEWAY_URL=https://other.example' > "$fixture/expected"
diff -u "$fixture/expected" "$CALL_LOG"
echo "PASS explicit gateway and device arguments remain separate"

sed '/^BUZZ_PUSH_GATEWAY_URL/d' "$config" > "$fixture/without-push"
mv "$fixture/without-push" "$config"
: > "$CALL_LOG"
/bin/bash "$device" phone > /dev/null
if grep -q -- --dart-define "$CALL_LOG"; then fail "unconfigured build enabled push"; fi
echo "PASS push stays optional"

for condition in host xcode dependencies identity arguments; do
    : > "$CALL_LOG"
    case "$condition" in
        host) export TEST_HOST_OS=Linux ;;
        xcode) export TEST_XCODE_EXIT=1 ;;
        dependencies) export TEST_PUB_EXIT=1 ;;
        identity) mv "$config" "$fixture/saved-config" ;;
    esac
    phone=phone
    [[ "$condition" != arguments ]] || phone=--release
    if /bin/bash "$device" "$phone" > /dev/null 2>&1; then fail "device command accepted $condition failure"; fi
    if grep -qx run "$CALL_LOG"; then fail "device command ran Flutter after $condition failure"; fi
    unset TEST_HOST_OS TEST_XCODE_EXIT TEST_PUB_EXIT
    [[ "$condition" != identity ]] || mv "$fixture/saved-config" "$config"
done
echo "PASS host, Xcode, dependency, identity, and argument failures stop installation"
