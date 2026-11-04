#!/usr/bin/env bash
# Wraps the release build in an AppImage, the package for Linux systems that
# do not take the .deb. Runs as root inside the Ubuntu container of
# scripts/package-deb.sh (BUZZ_GPUI_APPIMAGE=1), after the build, so the
# AppImage needs the glibc of that Ubuntu or later.
#
# The image carries the GStreamer the app plays media with, as Buzz.app does:
# the plugins below, the plugin scanner and every library they load that the
# AppImage exclude list does not leave to the system. AppRun points GStreamer
# at them, as crates/app/src/gst_bundle.rs does inside Buzz.app.
#
# Usage: scripts/build-appimage.sh <version>
#        -> dist/Buzz-<version>-linux-<x86_64|aarch64>.AppImage
set -euo pipefail
version="$1"
linuxdeploy_tag=1-alpha-20251107-1
appimagetool_tag=1.9.1
runtime_tag=20251108
arch="$(uname -m)"
case "$arch" in
  x86_64)
    linuxdeploy_sha256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
    appimagetool_sha256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
    runtime_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
    ;;
  aarch64)
    linuxdeploy_sha256=620095110d693282b8ebeb244a95b5e911cf8f65f76c88b4b47d16ae6346fcff
    appimagetool_sha256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
    runtime_sha256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
    ;;
  *) echo "No AppImage tools for $arch." >&2; exit 1 ;;
esac

# What playback, sounds and voice notes use: the set that
# scripts/bundle-macos-gstreamer.py bundles, with PulseAudio and ALSA in
# place of Core Audio, and without dav1d, which Ubuntu does not package.
plugins=(
  app audioconvert audioparsers audioresample autodetect coreelements
  pulseaudio alsa isomp4 libav matroska mpg123 ogg opus playback soup
  typefindfunctions videoconvertscale videoparsersbad volume vorbis vpx
  wavparse flac audiotestsrc rawparse id3demux pbtypes gio tcp
  adaptivedemux2 dash hls
)

here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"
release="${CARGO_TARGET_DIR:-$here/target}/release"
system="/usr/lib/$arch-linux-gnu"

apt-get install -y -qq --no-install-recommends \
  file patchelf gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-libav gstreamer1.0-alsa \
  gstreamer1.0-pulseaudio libsoup-3.0-0 glib-networking >/dev/null

tools="$(mktemp -d)"
fetch() {
  curl -sSfL -o "$tools/$1" "$2"
  echo "$3  $tools/$1" | sha256sum -c --quiet
  chmod +x "$tools/$1"
}
fetch linuxdeploy \
  "https://github.com/linuxdeploy/linuxdeploy/releases/download/$linuxdeploy_tag/linuxdeploy-$arch.AppImage" \
  "$linuxdeploy_sha256"
fetch appimagetool \
  "https://github.com/AppImage/appimagetool/releases/download/$appimagetool_tag/appimagetool-$arch.AppImage" \
  "$appimagetool_sha256"
# The static runtime: the AppImage then runs without libfuse2, which recent
# distributions no longer install.
fetch runtime \
  "https://github.com/AppImage/type2-runtime/releases/download/$runtime_tag/runtime-$arch" \
  "$runtime_sha256"
# The tools are AppImages themselves, and a container has no FUSE.
export APPIMAGE_EXTRACT_AND_RUN=1

appdir="$tools/Buzz.AppDir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/lib/gstreamer-1.0" "$appdir/usr/libexec/gstreamer-1.0" \
  "$appdir/usr/lib/gio/modules"
cp "$release/buzz-gpui" "$appdir/usr/bin/"
executables=(--executable "$appdir/usr/bin/buzz-gpui")
if [ "${BUZZ_GPUI_VOLTAGE:-0}" = 1 ]; then
  # The Sandbox host finds its provider beside the app.
  cp "$release/buzz-backend-relay" "$appdir/usr/bin/"
  executables+=(--executable "$appdir/usr/bin/buzz-backend-relay")
fi
for plugin in "${plugins[@]}"; do
  cp "$system/gstreamer-1.0/libgst$plugin.so" "$appdir/usr/lib/gstreamer-1.0/"
done
cp "$system/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner" "$appdir/usr/libexec/gstreamer-1.0/"
# GIO loads its TLS backend dynamically; libsoup's ELF dependencies do not
# include it. Keep it from the same distribution as the bundled GLib.
cp "$system/gio/modules/libgiognutls.so" "$appdir/usr/lib/gio/modules/"
"$system/glib-2.0/gio-querymodules" "$appdir/usr/lib/gio/modules"
icons=()
for size in 128 256 512; do
  mkdir -p "$tools/icons/$size"
  cp "assets/icons/buzz-$size.png" "$tools/icons/$size/buzz-gpui.png"
  icons+=(--icon-file "$tools/icons/$size/buzz-gpui.png")
done

# linuxdeploy copies in the libraries the executables, plugins and scanner
# load. The soup plugin opens libsoup by name, so it is named here.
"$tools/linuxdeploy" --appdir "$appdir" "${executables[@]}" \
  --deploy-deps-only "$appdir/usr/lib/gstreamer-1.0" \
  --deploy-deps-only "$appdir/usr/libexec/gstreamer-1.0" \
  --deploy-deps-only "$appdir/usr/lib/gio/modules" \
  --library "$system/libsoup-3.0.so.0" \
  --desktop-file packaging/linux/buzz-gpui.desktop "${icons[@]}"
# Plugins and the scanner find those libraries in usr/lib.
for plugin in "$appdir"/usr/lib/gstreamer-1.0/*.so; do
  patchelf --set-rpath "\$ORIGIN/.." "$plugin"
done
patchelf --set-rpath "\$ORIGIN/../../lib" "$appdir/usr/libexec/gstreamer-1.0/gst-plugin-scanner"
patchelf --set-rpath "\$ORIGIN/../.." "$appdir/usr/lib/gio/modules/libgiognutls.so"

# Node and Buzz's plugins, staged by scripts/package-deb.sh, where the app
# looks beside its binary. They go in after linuxdeploy: the Node release
# runs on any glibc this image does, and must not be rewritten.
bundle="${CARGO_TARGET_DIR:-$here/target}/release/bundle"
mkdir -p "$appdir/usr/lib/buzz-gpui" "$appdir/usr/share/buzz-gpui"
cp -r "$bundle/node" "$appdir/usr/lib/buzz-gpui/"
cp -r "$bundle/plugins" "$appdir/usr/share/buzz-gpui/"

# AppRun: GStreamer and GIO modules only from the image, with a registry of
# its own, so a system GStreamer is never mixed in or has its registry rewritten.
rm -f "$appdir/AppRun"
cat >"$appdir/AppRun" <<EOF
#!/bin/sh
here="\$(dirname "\$(readlink -f "\$0")")"
unset GST_PLUGIN_PATH GST_PLUGIN_PATH_1_0 GST_PLUGIN_SYSTEM_PATH
unset GIO_EXTRA_MODULES
export GIO_MODULE_DIR="\$here/usr/lib/gio/modules"
export GST_PLUGIN_SYSTEM_PATH_1_0="\$here/usr/lib/gstreamer-1.0"
export GST_PLUGIN_SCANNER_1_0="\$here/usr/libexec/gstreamer-1.0/gst-plugin-scanner"
export GST_REGISTRY_1_0="\${XDG_CACHE_HOME:-\$HOME/.cache}/buzz-gpui/gstreamer-registry-appimage-$arch.bin"
exec "\$here/usr/bin/buzz-gpui" "\$@"
EOF
chmod 755 "$appdir/AppRun"

out="dist/Buzz-$version-linux-$arch.AppImage"
rm -f "$out"
ARCH="$arch" "$tools/appimagetool" --no-appstream --runtime-file "$tools/runtime" "$appdir" "$out"
rm -rf "$tools"
