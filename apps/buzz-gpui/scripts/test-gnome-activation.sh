#!/usr/bin/env bash
# Checks that Buzz comes to the front on GNOME's Wayland session when it is
# meant to: from a notification click and from a second launch. Runs a
# headless GNOME Shell (the release Ubuntu 26.04 ships) with the app, a
# terminal holding focus, and the plain test relay for the message that
# notifies.
#
#   scripts/test-relay.sh up && scripts/test-relay.sh seed
#   cargo build -p buzz-gpui
#   scripts/test-gnome-activation.sh
#
# Screenshots and logs land in $OUT (default /tmp/buzz-gnome-activation).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${OUT:-/tmp/buzz-gnome-activation}
APP=${APP:-$ROOT/target/debug/buzz-gpui}
RELAY_DIR=${BUZZ_TEST_RELAY:-$ROOT/.test-relay}

if [[ ${1:-} != --inside ]]; then
  build() { nix build --no-link --print-out-paths "nixpkgs#$1" | tail -1; }
  rm -rf "$OUT" && mkdir -p "$OUT"
  export GS=$(build gnome-shell) MESA=$(build mesa) FOOT=$(build foot)
  export GLIB=$(build glib.bin) DBUS=$(build dbus) SQLITE=$(build sqlite.bin)
  export APPINDICATOR=$(build gnomeExtensions.appindicator)
  export OUT APP RELAY_DIR
  # The app needs the dev shell's libraries; everything else runs on a
  # private session bus.
  exec nix develop "$ROOT" --command "$DBUS/bin/dbus-run-session" -- "$0" --inside
fi

export PATH=$GLIB/bin:$PATH
export HOME=$OUT/home XDG_CONFIG_HOME=$OUT/home/.config XDG_DATA_HOME=$OUT/home/.local/share
export XDG_RUNTIME_DIR=$OUT/runtime GSETTINGS_BACKEND=keyfile
export WAYLAND_DISPLAY=buzz-test
unset DISPLAY XDG_ACTIVATION_TOKEN DESKTOP_STARTUP_ID
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/glib-2.0/settings" && chmod 700 "$XDG_RUNTIME_DIR"

# An extension that turns on unsafe mode, so this script can drive the
# shell through Eval.
EXT=$XDG_DATA_HOME/gnome-shell/extensions/buzz-test@local
mkdir -p "$EXT"
cat > "$EXT/metadata.json" <<'EOF'
{"uuid": "buzz-test@local", "name": "Buzz test", "description": "Unsafe mode for tests", "shell-version": ["50"]}
EOF
cat > "$EXT/extension.js" <<'EOF'
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
export default class BuzzTest extends Extension {
    enable() { global.context.unsafe_mode = true; }
    disable() {}
}
EOF
cat > "$XDG_CONFIG_HOME/glib-2.0/settings/keyfile" <<'EOF'
[org/gnome/shell]
enabled-extensions=['buzz-test@local', 'appindicatorsupport@rgcjonas.gmail.com']
welcome-dialog-last-shown-version='999'

[org/gnome/desktop/interface]
enable-hot-corners=false
EOF

# Ubuntu turns on its AppIndicator extension, which shows tray icons.
ln -s "$APPINDICATOR/share/gnome-shell/extensions/appindicatorsupport@rgcjonas.gmail.com" \
  "$XDG_DATA_HOME/gnome-shell/extensions/"

# Installed as the .deb installs them, so banners carry Buzz's name and icon.
mkdir -p "$XDG_DATA_HOME/applications" "$XDG_DATA_HOME/icons/hicolor/256x256/apps"
sed "s|^Exec=buzz-gpui|Exec=$APP|" "$ROOT/packaging/linux/buzz-gpui.desktop" \
  > "$XDG_DATA_HOME/applications/buzz-gpui.desktop"
cp "$ROOT/assets/icons/buzz-256.png" "$XDG_DATA_HOME/icons/hicolor/256x256/apps/buzz-gpui.png"

MUTTER_DEBUG=${MUTTER_DEBUG:-} LIBGL_ALWAYS_SOFTWARE=1 __EGL_VENDOR_LIBRARY_FILENAMES=$MESA/share/glvnd/egl_vendor.d/50_mesa.json \
  "$GS/bin/gnome-shell" --headless --wayland --no-x11 --virtual-monitor 1400x900 \
  --wayland-display "$WAYLAND_DISPLAY" > "$OUT/shell.log" 2>&1 &
SHELL_PID=$!
PIDS=($SHELL_PID)
trap 'kill "${PIDS[@]}" 2>/dev/null || true' EXIT

shell_eval() {
  local reply
  reply=$(gdbus call --session -d org.gnome.Shell -o /org/gnome/Shell -m org.gnome.Shell.Eval "$1")
  if [[ $reply != "(true, "* ]]; then
    echo "Eval failed: $reply" >&2
    return 1
  fi
  # (true, '<JSON>') → the value, unquoted when it is a string.
  reply=${reply#*, \'}
  reply=${reply%\')}
  python3 -c 'import json, sys; v = json.loads(sys.argv[1]); print(v if isinstance(v, str) else json.dumps(v))' "$reply"
}
wait_for() { # description, JS that must yield 'true', [seconds]
  for _ in $(seq 1 $(( ${3:-30} * 2 ))); do
    [[ $(shell_eval "$2" 2>/dev/null) == true ]] && return 0
    sleep 0.5
  done
  echo "FAIL: timed out waiting for $1"
  exit 1
}
focused() { shell_eval 'String(global.display.focus_window?.get_wm_class())'; }
screenshot() {
  gdbus call --session -d org.gnome.Shell.Screenshot -o /org/gnome/Shell/Screenshot \
    -m org.gnome.Shell.Screenshot.Screenshot false false "$OUT/$1.png" > /dev/null
}
focus_terminal() {
  shell_eval 'const w = global.get_window_actors().map(a => a.meta_window).find(w => w.get_wm_class() === "foot"); Main.activateWindow(w); true' > /dev/null
  wait_for "the terminal to take focus" 'String(global.display.focus_window?.get_wm_class()) === "foot"'
}
ready_notices() {
  shell_eval 'String(Main.messageTray.getSources().flatMap(s => s.notifications).filter(n => /is ready/.test(n.title)).length)'
}

for _ in $(seq 1 60); do
  [[ $(shell_eval 'global.context.unsafe_mode' 2>/dev/null) == true ]] && break
  sleep 1
done
shell_eval 'Main.overview.hide(); true' > /dev/null
# GNOME warns, and keeps warning, that the test put it in unsafe mode; the
# warning would hold up the banners behind it.
sleep 2
shell_eval 'Main.messageTray.getSources().forEach(s => s.destroy()); true' > /dev/null

# The app as alice, on #general of the plain test relay.
ALICE_NSEC=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['identities']['alice']['nsec'])" "$RELAY_DIR/identities.json")
ALICE=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['identities']['alice']['pk_hex'])" "$RELAY_DIR/identities.json")
BOB_NSEC=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['identities']['bob']['nsec'])" "$RELAY_DIR/identities.json")
GENERAL=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['channels']['general'])" "$RELAY_DIR/seed.json")
DM=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['channels']['dm-alice-bob'])" "$RELAY_DIR/seed.json")
echo "$ALICE_NSEC" > "$OUT/alice.nsec"
mkdir -p "$XDG_CONFIG_HOME/buzz-gpui" "$XDG_DATA_HOME/buzz-gpui"
printf '[embedded]\ncredentials = "files"\nworkspace = "%s/ws"\n' "$OUT" > "$XDG_CONFIG_HOME/buzz-gpui/config.toml"
export BUZZ_DEV_NSEC_FILE=$OUT/alice.nsec
export VK_ICD_FILENAMES=$MESA/share/vulkan/icd.d/lvp_icd.x86_64.json

"$APP" > "$OUT/app-seed.log" 2>&1 & SEED=$!
sleep 4 && kill $SEED && wait $SEED 2>/dev/null || true
"$SQLITE/bin/sqlite3" "$XDG_DATA_HOME/buzz-gpui/buzz.sqlite3" \
  "INSERT OR REPLACE INTO viewer_prefs VALUES ('$ALICE','communities','{\"origins\":[\"ws://127.0.0.1:53000\"],\"selected\":\"ws://127.0.0.1:53000\"}');
   INSERT OR REPLACE INTO view_state VALUES ('http://127.0.0.1:53000','$ALICE','selected-channel','\"$GENERAL\"',0);
   INSERT OR REPLACE INTO device_prefs VALUES ('tray','{\"show_icon\":true,\"minimize_to_tray\":true,\"close_to_tray\":true}');"

"$APP" > "$OUT/app.log" 2>&1 & APP_PID=$!
PIDS+=($APP_PID)
wait_for "the Buzz window" 'global.get_window_actors().some(a => a.meta_window.get_wm_class() === "buzz-gpui")'
sleep 3
"$FOOT/bin/foot" > "$OUT/foot.log" 2>&1 & PIDS+=($!)
wait_for "the terminal window" 'global.get_window_actors().some(a => a.meta_window.get_wm_class() === "foot")'
focus_terminal

fail=0
check() { # name, expected focus
  local got
  got=$(focused)
  if [[ $got == "$2" ]]; then echo "ok: $1 (focus: $got)"; else echo "FAIL: $1 (focus: $got, expected $2)"; fail=1; fi
}

# 1. Control: a second launch without a token cannot take focus. If this
#    fails the session is not refusing focus requests, and the checks
#    below prove nothing.
"$APP" > "$OUT/second-plain.log" 2>&1
sleep 2
check "a second launch without a token stays behind" foot
echo "   'is ready' notices so far: $(ready_notices)"

# 2. A second launch with a token, as GNOME hands one to what it starts.
focus_terminal
# Stamped with the current time, as a launch from a click would be; a
# token made outside any input event has time 0 and has already expired.
TOKEN=$(shell_eval 'global.create_app_launch_context(global.display.get_current_time_roundtrip(), -1).get_startup_notify_id(null, [])')
XDG_ACTIVATION_TOKEN=$TOKEN "$APP" > "$OUT/second-token.log" 2>&1
sleep 2
check "a second launch with a token comes to the front" buzz-gpui
screenshot second-launch

# 3. A direct message from bob notifies; clicking the banner opens it.
send_dm() {
  STAMP=$(date +%s%N)
  BUZZ_RELAY_URL=http://127.0.0.1:53000 BUZZ_PRIVATE_KEY=$BOB_NSEC \
    "$RELAY_DIR/buzz/target/debug/buzz" messages send --channel "$DM" \
    --content "activation test $STAMP" >> "$OUT/send.log"
}
focus_terminal
send_dm
wait_for "the banner" 'Main.messageTray.getSources().some(s => s.notifications.some(n => /sent you a direct message/.test(n.title)))'
screenshot banner
# Real clicks, through a virtual pointer: GNOME takes a token's time from
# the input event it is handling.
click() { # JS for an actor, [clicks]
  shell_eval "
    const {Clutter, GLib} = imports.gi;
    const actor = $1;
    const [x, y] = actor.get_transformed_position();
    const [w, h] = actor.get_transformed_size();
    const seat = global.stage.context.get_backend().get_default_seat();
    globalThis.buzzPointer ??= seat.create_virtual_device(Clutter.InputDeviceType.POINTER_DEVICE);
    buzzPointer.notify_absolute_motion(GLib.get_monotonic_time(), x + w / 3, y + h / 2);
    true" > /dev/null
  sleep 0.3
  for _ in $(seq 1 "${2:-1}"); do
    shell_eval '
      const {Clutter, GLib} = imports.gi;
      buzzPointer.notify_button(GLib.get_monotonic_time(), Clutter.BUTTON_PRIMARY, Clutter.ButtonState.PRESSED);
      buzzPointer.notify_button(GLib.get_monotonic_time() + 20000, Clutter.BUTTON_PRIMARY, Clutter.ButtonState.RELEASED);
      true' > /dev/null
    sleep 0.1
  done
}
click_banner() {
  # Unread messages from earlier runs notify too and queue ahead of it.
  wait_for "this message's banner" "Main.messageTray._banner?.notification?.body?.includes('$STAMP') === true" 120
  # Where the banner rests once it has slid in.
  wait_for "the banner to settle" '
    const banner = Main.messageTray._banner;
    const at = banner?.mapped ? banner.get_transformed_position().join() : "";
    const settled = at !== "" && at === globalThis.buzzBannerAt;
    globalThis.buzzBannerAt = at;
    settled'
  click 'Main.messageTray._banner'
}
click_banner
sleep 2
check "clicking the banner brings Buzz to the front" buzz-gpui
echo "   banners left: $(shell_eval 'String(Main.messageTray.getSources().flatMap(s => s.notifications).filter(n => /sent you a direct message/.test(n.title)).length)')"
screenshot clicked

# 4. Files copied in a file manager paste into the composer as attachments.
cp "$ROOT/assets/icons/buzz-128.png" "$OUT/copied-file.png"
shell_eval "
  const {Clutter, GLib, St} = imports.gi;
  const list = new TextEncoder().encode('file://$OUT/copied-file.png' + String.fromCharCode(13, 10));
  St.Clipboard.get_default().set_content(St.ClipboardType.CLIPBOARD, '${PASTE_MIME:-text/uri-list}', new GLib.Bytes(list));
  true" > /dev/null
sleep 0.5
# Keys go through a virtual keyboard kept alive across calls.
key() { # keyval name, pressed|released
  shell_eval "
    const {Clutter, GLib} = imports.gi;
    globalThis.buzzKeyboard ??= global.stage.context.get_backend().get_default_seat()
      .create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
    buzzKeyboard.notify_keyval(GLib.get_monotonic_time(), Clutter.KEY_$1,
      Clutter.KeyState.${2^^});
    true" > /dev/null
  sleep 0.1
}
key Control_L pressed; key v pressed; key v released; key Control_L released
sleep 2
screenshot pasted
echo "   look at $OUT/pasted.png: the composer should hold copied-file.png as an attachment"

# 5. The tray icon, in Ubuntu's AppIndicator extension.
TRAY='Object.values(Main.panel.statusArea).find(b => b._indicator?.id === "buzz-gpui")'
wait_for "the tray icon" "Boolean($TRAY)"
echo "ok: the tray icon is up"
screenshot tray

# 6. With close-to-tray chosen, closing the window keeps Buzz running.
BUZZ_GONE='!global.get_window_actors().some(a => a.meta_window.get_wm_class() === "buzz-gpui")'
close_window() {
  shell_eval 'global.get_window_actors().map(a => a.meta_window).find(w => w.get_wm_class() === "buzz-gpui").delete(global.get_current_time()); true' > /dev/null
  wait_for "the window to close" "$BUZZ_GONE"
  sleep 1
}
close_window
if kill -0 "$APP_PID" 2> /dev/null; then
  echo "ok: closing the window keeps Buzz running in the tray"
else
  echo "FAIL: Buzz quit when its window closed"
  exit 1
fi

# 7. "Open Buzz" in the tray menu brings the window back, in front.
open_menu() {
  click "$TRAY"
  wait_for "the tray menu" "$TRAY.menu.isOpen === true"
  sleep 0.5
}
menu_item() { echo "$TRAY.menu._getMenuItems().find(i => i.label?.text === '$1')"; }
focus_terminal
open_menu
click "$(menu_item 'Open Buzz')"
wait_for "the window to come back" "!($BUZZ_GONE)"
sleep 2
check "Open Buzz in the tray menu brings it back in front" buzz-gpui

# 8. Hidden, a notification click opens it on the message.
close_window
focus_terminal
send_dm
click_banner
wait_for "the window to come back" "!($BUZZ_GONE)"
sleep 2
check "a notification click while hidden brings it back in front" buzz-gpui
screenshot reopened-on-message

# 9. Double-clicking the icon opens it too.
close_window
focus_terminal
click "$TRAY" 2
wait_for "the window to come back" "!($BUZZ_GONE)"
sleep 2
check "double-clicking the tray icon brings it back in front" buzz-gpui

# 10. Quit Buzz in the tray menu quits.
open_menu
click "$(menu_item 'Quit Buzz')"
for _ in $(seq 1 20); do kill -0 "$APP_PID" 2> /dev/null || break; sleep 0.5; done
if kill -0 "$APP_PID" 2> /dev/null; then
  echo "FAIL: Quit Buzz left it running"
  fail=1
else
  echo "ok: Quit Buzz in the tray menu quits"
fi

exit $fail
