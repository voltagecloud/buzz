#!/usr/bin/env bash
# Drives the dev server's persistent headless GNOME desktop (the
# gui-testing user service in the NixOS config): launch the app into it,
# take screenshots, and run shell JavaScript through org.gnome.Shell.Eval.
#
#   scripts/desktop.sh run [args...]   start target/debug/buzz-gpui (or $APP)
#   scripts/desktop.sh stop            stop it
#   scripts/desktop.sh shot [file]     screenshot (default /tmp/desktop.png)
#   scripts/desktop.sh eval 'js'       evaluate in GNOME Shell
#   scripts/desktop.sh status          is the desktop up
#
# The app runs in the dev shell as the user unit $DESKTOP_UNIT (default
# desktop-app), with its log at /tmp/$DESKTOP_UNIT.log; give each concurrent
# user its own unit. VK_DRIVER_FILES and friends pass through, so a busy or
# faulty GPU can be left out of Vulkan's device probe. The
# session has no secret service: sign in with BUZZ_DEV_NSEC_FILE, and set
# HOME (or the XDG dirs) to keep test data out of your own profile.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
APP=${APP:-$ROOT/target/debug/buzz-gpui}
export DESKTOP_UNIT=${DESKTOP_UNIT:-desktop-app}
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/gui-testing-bus

shell_call() {
  gdbus call --session -d org.gnome.Shell -o "/org/gnome/$1" -m "org.gnome.$2" "${@:3}"
}

case ${1:-} in
  run)
    shift
    systemctl --user is-active -q gui-testing || {
      echo "gui-testing is not running: systemctl --user start gui-testing" >&2
      exit 1
    }
    systemctl --user stop "$DESKTOP_UNIT" 2>/dev/null || true
    systemctl --user reset-failed "$DESKTOP_UNIT" 2>/dev/null || true
    # nix develop resolves the dev shell's libraries; its environment then
    # carries into the transient unit. Unset variables stay unset.
    nix develop "$ROOT" --command bash -c '
      env=(-E WAYLAND_DISPLAY=gui-testing)
      # The unit has no dev shell PATH: Node plugins use the shell'"'"'s node.
      [[ -v BUZZ_NODE ]] || export BUZZ_NODE=$(command -v node || true)
      for var in BUZZ_NODE BUZZ_BUNDLED_PLUGINS DBUS_SESSION_BUS_ADDRESS LD_LIBRARY_PATH XDG_DATA_DIRS \
        BUZZ_DEV_NSEC_FILE BUZZ_DEV_PICK_FILE BUZZ_DEV_OPEN_LINK BUZZODZ_SAFE_MODE SSL_CERT_FILE \
        BUZZ_UPDATE_URL BUZZ_UPDATE_KIND BUZZ_UPDATE_INSTALLER \
        VK_DRIVER_FILES VK_ICD_FILENAMES VK_LOADER_DRIVERS_DISABLE \
        RUST_LOG HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME; do
        [[ -v $var ]] && env+=(-E "$var")
      done
      exec systemd-run --user --unit="$DESKTOP_UNIT" --collect --same-dir "${env[@]}" \
        -p StandardOutput=file:/tmp/$DESKTOP_UNIT.log -p StandardError=inherit "$@"
    ' _ "$APP" "$@"
    ;;
  stop) systemctl --user stop "$DESKTOP_UNIT" ;;
  shot)
    file=${2:-/tmp/desktop.png}
    shell_call Shell/Screenshot Shell.Screenshot.Screenshot false false "$file" >/dev/null
    echo "$file"
    ;;
  eval) shell_call Shell Shell.Eval "$2" ;;
  status) systemctl --user status gui-testing --no-pager | head -3 ;;
  *) sed -n '2,12p' "$0" >&2; exit 2 ;;
esac
