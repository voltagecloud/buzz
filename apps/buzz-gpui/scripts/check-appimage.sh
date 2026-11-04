#!/usr/bin/env bash
# Checks that an AppImage from scripts/build-appimage.sh runs on Linux
# systems other than the one it was built on, with only what it carries and
# the libraries every desktop has (the ones the AppImage exclude list leaves
# to the system: X11, fontconfig, ALSA, ...). In a container of each system
# below, which has no GStreamer:
#
# 1. Every ELF in the image finds every library it loads.
# 2. The bundled Node runs every bundled plugin (and the provider runs, in the
#    Voltage build).
# 3. `buzz-gpui --gst-check` finds every element the app uses from the
#    bundled GStreamer, encodes a voice note and plays it and the test clips,
#    including HTTPS with the bundled GIO TLS backend.
#
# Usage: scripts/check-appimage.sh dist/Buzz-<version>-linux-<arch>.AppImage
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
appimage="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
media="$(mktemp -d)"
trap 'rm -rf "$media"' EXIT
cp "$here"/crates/relay/tests/fixtures/clip.{webm,mov} "$here/assets/sounds/ping.mp3" "$media/"
# A private test CA, trusted only in the disposable containers. Playback must
# verify the HTTPS certificate; no network service or disabled TLS checks.
openssl req -x509 -newkey rsa:2048 -noenc -days 1 \
  -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
  -keyout "$media/key.pem" -out "$media/ca.crt" >/dev/null 2>&1

# The oldest supported Ubuntu (the build's own), and two other families,
# each with the desktop libraries the exclude list leaves to the system.
debian_baseline=(
  libx11-6 libx11-xcb1 libxcb1 libxcb-dri3-0 libdrm2 libgl1 libegl1 libvulkan1
  libwayland-client0 libfontconfig1 libfreetype6 libharfbuzz0b libfribidi0
  libasound2t64 libgpg-error0 libcom-err2 libdbus-1-3 ca-certificates
)
fedora_baseline=(
  libX11 libX11-xcb libxcb libdrm mesa-libGL mesa-libEGL vulkan-loader
  libwayland-client fontconfig freetype harfbuzz fribidi alsa-lib
  libgpg-error libcom_err dbus-libs ca-certificates
)
apt="apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends ${debian_baseline[*]}"
systems=(
  "ubuntu:24.04|$apt"
  "debian:13|$apt"
  "fedora:43|dnf install -y -q --setopt=install_weak_deps=False ${fedora_baseline[*]}"
)

# Runs in each container, as root.
inside=$(cat <<'EOF'
set -eu
sh -c "$INSTALL" >/dev/null
if ldconfig -p | grep -q libgstreamer-1.0; then
  echo "FAIL this system has a GStreamer of its own"
  exit 1
fi
if find /usr/lib /usr/lib64 -name libgiognutls.so -print 2>/dev/null | grep -q .; then
  echo "FAIL this system has a GIO TLS backend of its own"
  exit 1
fi
if command -v update-ca-certificates >/dev/null; then
  cp /check/media/ca.crt /usr/local/share/ca-certificates/buzz-check.crt
  update-ca-certificates >/dev/null 2>&1
else
  cp /check/media/ca.crt /etc/pki/ca-trust/source/anchors/buzz-check.crt
  update-ca-trust
fi
cd /tmp
/check/Buzz.AppImage --appimage-extract >/dev/null
root=/tmp/squashfs-root
missing=$(find "$root" -type f | while read -r file; do
  if [ "$(head -c4 "$file" | tail -c3)" = ELF ]; then
    ldd "$file" 2>/dev/null | awk -v file="${file#"$root"/}" '/not found/ { print file ": " $1 }'
  fi
done)
if [ -n "$missing" ]; then
  echo "FAIL libraries not found:"
  echo "$missing"
  exit 1
fi
echo "every ELF finds its libraries"
node="$root/usr/lib/buzz-gpui/node/bin/node"
"$node" /check/scripts/check-plugins.mjs "$root/usr/share/buzz-gpui/plugins"
if [ -f "$root/usr/bin/buzz-backend-relay" ]; then
  "$node" /check/scripts/check-remote-provider.mjs "$root/usr/bin/buzz-backend-relay"
fi
# Use the image's Node and only its standard library for the HTTPS fixture.
"$node" --input-type=module - <<'JS' &
import { createServer } from 'node:https';
import { readFileSync, writeFileSync } from 'node:fs';
const media = readFileSync('/check/media/clip.webm');
createServer({ key: readFileSync('/check/media/key.pem'), cert: readFileSync('/check/media/ca.crt') },
  (_request, response) => {
    response.writeHead(200, { 'Content-Type': 'video/webm', 'Content-Length': media.length });
    response.end(media);
  }).listen(8443, '127.0.0.1', () => writeFileSync('/tmp/https-ready', 'ready'));
JS
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT
attempt=0
until [ -f /tmp/https-ready ]; do
  if ! kill -0 "$server" 2>/dev/null || [ "$attempt" -ge 30 ]; then
    echo "FAIL HTTPS fixture did not start"
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 1
done
APPIMAGE_EXTRACT_AND_RUN=1 HOME=/tmp/home /check/Buzz.AppImage --gst-check \
  file:///check/media/clip.webm file:///check/media/ping.mp3 file:///check/media/clip.mov \
  https://127.0.0.1:8443/clip.webm
EOF
)

for system in "${systems[@]}"; do
  image="${system%%|*}"
  echo "== $image"
  docker run --rm \
    -v "$appimage:/check/Buzz.AppImage:ro" \
    -v "$media:/check/media:ro" \
    -v "$here/scripts:/check/scripts:ro" \
    -e INSTALL="${system#*|}" \
    "$image" sh -c "$inside"
done
