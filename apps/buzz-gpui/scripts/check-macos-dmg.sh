#!/usr/bin/env bash
# Checks that the Buzz.app in a .dmg from scripts/package-macos.sh plays
# media with only what it carries:
#
# 1. No Mach-O in the bundle refers to a library outside it, other than
#    the system's (/usr/lib, /System).
# 2. With the GStreamer framework moved out of the way and GST_* variables
#    pointing at it, `buzz-gpui --gst-check` finds every element the app
#    uses, encodes a voice note and plays it and the test clips (WebM and
#    MP3 over HTTP, QuickTime from a file, whose index is at the end), while
#    dyld reports loading nothing from outside the bundle and the system.
#
# For CI: it moves /Library/Frameworks/GStreamer.framework (with sudo) and
# puts it back when done.
#
# Usage: scripts/check-macos-dmg.sh dist/Buzz-<version>-macos-<arch>.dmg
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
dmg="$1"
# Its real path (not /var/...), the one dyld reports.
work="$(cd "$(mktemp -d)" && pwd -P)"
framework=/Library/Frameworks/GStreamer.framework
hidden="$work/GStreamer.framework"
server=""

cleanup() {
  [ -n "$server" ] && kill "$server" 2>/dev/null || true
  if [ -d "$hidden" ]; then
    sudo mv "$hidden" "$framework"
  fi
  rm -rf "$work"
}
trap cleanup EXIT

mount="$work/mount"
mkdir "$mount"
hdiutil attach -nobrowse -readonly -mountpoint "$mount" "$dmg" >/dev/null
cp -R "$mount/Buzz.app" "$work/"
hdiutil detach "$mount" >/dev/null
app="$work/Buzz.app"

# The bundled Node runs every bundled plugin.
"$app/Contents/Resources/node/bin/node" "$here/scripts/check-plugins.mjs" \
  "$app/Contents/Resources/plugins"
du -sh "$app"
codesign --verify --deep --strict "$app"

echo "== Libraries every Mach-O in the bundle loads"
bad=0
while IFS= read -r -d '' file; do
  if ! file -b "$file" | grep -q Mach-O; then
    continue
  fi
  # The first line names the file; a dylib's own install name follows.
  refs="$(otool -L "$file" | tail -n +2 | awk '{print $1}')"
  if foreign="$(grep -Ev '^(@rpath/|@loader_path/|@executable_path/|/usr/lib/|/System/)' <<<"$refs")"; then
    echo "FAIL ${file#"$work"/} loads: $foreign"
    bad=1
  fi
  if grep -E '^(/opt/homebrew|/usr/local|/Library/Frameworks)' <<<"$refs"; then
    echo "FAIL ${file#"$work"/} loads from Homebrew or /Library/Frameworks"
    bad=1
  fi
  rpaths="$(otool -l "$file" | awk '/cmd LC_RPATH/ {getline; getline; print $2}')"
  if [ -n "$rpaths" ] && grep -Ev '^@(loader|executable)_path' <<<"$rpaths"; then
    echo "FAIL ${file#"$work"/} has an rpath outside the bundle"
    bad=1
  fi
done < <(find "$app" -type f -print0)
[ "$bad" = 0 ] || exit 1
echo "only bundled and system libraries"

echo "== The bundled GStreamer, with no other on the machine"
if [ -d "$framework" ]; then
  sudo mv "$framework" "$hidden"
fi
media="$work/media"
mkdir "$media"
cp "$here"/crates/relay/tests/fixtures/clip.{webm,mov} "$here/assets/sounds/ping.mp3" "$media/"
port=8731
python3 -m http.server --bind 127.0.0.1 --directory "$media" "$port" >/dev/null 2>&1 &
server=$!
# No "Terminated" notice from bash when cleanup stops it.
disown "$server"
for _ in $(seq 50); do
  curl -fs "http://127.0.0.1:$port/" >/dev/null && break
  sleep 0.2
done

# A clean environment, except for variables that would pull in the
# GStreamer moved aside if the app honoured them.
log="$work/check.log"
status=0
mkdir "$work/home"
elsewhere="$hidden/Versions/1.0/lib"
env -i HOME="$work/home" TMPDIR="$work" PATH=/usr/bin:/bin \
  GST_PLUGIN_PATH="$elsewhere/gstreamer-1.0" \
  GST_PLUGIN_SYSTEM_PATH="$elsewhere/gstreamer-1.0" \
  GST_PLUGIN_PATH_1_0="$elsewhere/gstreamer-1.0" \
  GIO_EXTRA_MODULES="$elsewhere/gio/modules" \
  DYLD_PRINT_LIBRARIES=1 \
  "$app/Contents/MacOS/buzz-gpui" --gst-check \
  "http://127.0.0.1:$port/clip.webm" \
  "http://127.0.0.1:$port/ping.mp3" \
  "file://$media/clip.mov" >"$log" 2>&1 || status=$?
grep -v '^dyld\[' "$log"
[ "$status" = 0 ] || { echo "the self-check exited with $status"; exit 1; }

echo "== Images dyld loaded"
# dyld prints rpath-expanded paths (Contents/MacOS/../Frameworks/...);
# normalize them before comparing.
loaded="$(sed -n 's/^dyld\[[0-9]*\]:[^/]*//p' "$log" \
  | python3 -c 'import os, sys; [print(os.path.normpath(l.strip())) for l in sys.stdin if l.strip()]' \
  | sort -u)"
if ! grep -q "^$app/Contents/Frameworks/libgstreamer-1.0.0.dylib$" <<<"$loaded"; then
  echo "FAIL dyld did not report loading the bundled libgstreamer"
  echo "$(grep -c '^dyld\[' "$log") dyld lines; the first:"
  grep '^dyld\[' "$log" | head -5
  exit 1
fi
# /usr/bin/arch: GStreamer starts the plugin scanner through it on Intel.
if outside="$(grep -Ev "^($app/|/usr/lib/|/usr/bin/|/System/)" <<<"$loaded")"; then
  echo "FAIL loaded from outside the bundle and the system:"
  echo "$outside"
  exit 1
fi
echo "$(grep -c "^$app/" <<<"$loaded") bundled images loaded, nothing else outside the system"
