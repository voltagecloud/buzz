#!/usr/bin/env bash
# Builds an ad-hoc signed Buzz.app and wraps it in a .dmg. Runs on macOS,
# for the architecture of the machine it runs on. The app carries the
# GStreamer it plays media with, copied from the official GStreamer
# framework (the runtime and development .pkg installers from
# gstreamer.freedesktop.org), so users need not install GStreamer.
#
# The app also carries Buzz's own Node plugins (plugins/) and the Node that
# runs them, pinned below and checked against its published checksum.
#
# Usage: scripts/package-macos.sh   -> dist/Buzz-<version>-macos-<arch>.dmg
# GSTREAMER_PREFIX overrides where the framework is.
set -euo pipefail
node_version=22.23.3
node_sha256_arm64=23b25245dcfb9af7262f8ff142e9e2e0af025368117329e7a7458a51e5922f53
node_sha256_x86_64=8a677b0219178efd6eb0e475457c4afb452b521a92f6e67845a73bd85727f2a8
here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"

gst="${GSTREAMER_PREFIX:-/Library/Frameworks/GStreamer.framework/Versions/1.0}"
if [ ! -d "$gst/lib/pkgconfig" ]; then
  echo "No GStreamer framework with development files at $gst." >&2
  echo "Install the runtime and devel .pkg from gstreamer.freedesktop.org." >&2
  exit 1
fi
# Build against the framework, not any other GStreamer (Homebrew's).
export PKG_CONFIG_PATH="$gst/lib/pkgconfig"
# Its libraries are @rpath/<name>; the app finds them in Contents/Frameworks.
# The header padding leaves install_name_tool room to rewrite them.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-headerpad_max_install_names"

version="$(cargo metadata --no-deps --format-version 1 \
  | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "buzz-gpui"))')"
arch="$(uname -m)"
cargo build --locked --release -p buzz-gpui

work="$(mktemp -d)"
app="$work/Buzz.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" dist
cp target/release/buzz-gpui "$app/Contents/MacOS/"

iconset="$work/buzz.iconset"
mkdir "$iconset"
for size in 128 256 512; do
  cp "assets/icons/buzz-$size.png" "$iconset/icon_${size}x${size}.png"
  if [ "$size" -lt 512 ]; then
    cp "assets/icons/buzz-$((size * 2)).png" "$iconset/icon_${size}x${size}@2x.png"
  fi
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/buzz.icns"

cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Buzz</string>
  <key>CFBundleDisplayName</key><string>Buzz</string>
  <key>CFBundleIdentifier</key><string>com.benthecarman.buzz-gpui</string>
  <key>CFBundleExecutable</key><string>buzz-gpui</string>
  <key>CFBundleIconFile</key><string>buzz</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>Buzz records voice notes.</string>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key><string>Buzz link</string>
      <key>CFBundleURLSchemes</key><array><string>buzz</string></array>
    </dict>
  </array>
</dict>
</plist>
EOF

python3 scripts/bundle-macos-gstreamer.py "$app" "$gst" "$arch"

# Node, for the plugins and to build them.
case "$arch" in
  arm64) node_arch=arm64 node_sha256=$node_sha256_arm64 ;;
  x86_64) node_arch=x64 node_sha256=$node_sha256_x86_64 ;;
  *) echo "No Node for $arch." >&2; exit 1 ;;
esac
node_tarball="$work/node.tar.gz"
curl -sSfL -o "$node_tarball" \
  "https://nodejs.org/dist/v$node_version/node-v$node_version-darwin-$node_arch.tar.gz"
echo "$node_sha256  $node_tarball" | shasum -a 256 -c --quiet
mkdir "$work/node"
tar -xzf "$node_tarball" -C "$work/node" --strip-components=1
mkdir -p "$app/Contents/Resources/node/bin" "$app/Contents/Resources/plugins"
cp "$work/node/bin/node" "$app/Contents/Resources/node/bin/node"
cp "$work/node/LICENSE" "$app/Contents/Resources/node/LICENSE"
# Build the plugins in a copy, so the checkout is left as it was.
rsync -a --exclude node_modules --exclude dist plugins/ "$work/plugins/"
(cd "$work/plugins" && PATH="$work/node/bin:$PATH" npm ci --no-audit --no-fund --loglevel=error \
  && PATH="$work/node/bin:$PATH" npm run build)
for manifest in "$work"/plugins/*/buzz-plugin.json; do
  folder=$(dirname "$manifest")
  mkdir -p "$app/Contents/Resources/plugins/$(basename "$folder")"
  cp -R "$manifest" "$folder/dist" "$app/Contents/Resources/plugins/$(basename "$folder")/"
done
rm -rf "$work/node" "$work/plugins" "$node_tarball"

# Ad-hoc signature: Apple Silicon refuses to run unsigned code at all, and
# install_name_tool broke the signatures the copies came with. Each
# library, plugin and helper first, then the app around them (signing the
# main executable signs the app).
find "$app/Contents" -type f ! -path "$app/Contents/MacOS/buzz-gpui" \
  \( -name '*.dylib' -o -name '*.so' -o -perm -u+x \) -print0 \
  | while IFS= read -r -d '' file; do
      if file -b "$file" | grep -q Mach-O; then
        # Quiet about the signatures it replaces, loud about failures.
        if ! out="$(codesign --force --sign - "$file" 2>&1)"; then
          echo "$out" >&2
          exit 1
        fi
      fi
    done
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

dmg="dist/Buzz-$version-macos-$arch.dmg"
rm -f "$dmg"
ln -s /Applications "$work/Applications"
rm -rf "$iconset"
# hdiutil's own estimate of the image size can fall short ("No space left
# on device"), so give it the contents' size and room to spare.
size_mb="$(du -sm "$work" | cut -f1)"
hdiutil create -volname Buzz -srcfolder "$work" -size "$((size_mb * 5 / 4 + 20))m" \
  -ov -format UDZO "$dmg"
rm -rf "$work"
ls -la "$dmg"
