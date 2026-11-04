#!/usr/bin/env bash
# Builds the Ubuntu .deb inside an ubuntu:24.04 container, so the binary links
# against Ubuntu's libraries rather than Nix store paths, and needs only the
# glibc of Ubuntu 24.04 or Debian 13 and later. Builds for the architecture
# of the machine it runs on.
#
# Usage: scripts/package-deb.sh   -> dist/buzz-gpui_<version>_<amd64|arm64>.deb
#
# BUZZ_GPUI_APPIMAGE=1 also wraps the same build in an AppImage, for other
# distributions: dist/Buzz-<version>-linux-<x86_64|aarch64>.AppImage (see
# scripts/build-appimage.sh).
#
# BUZZ_GPUI_RELEASE_VERSION, when set, is the package version and the version
# the app reports.
#
# The package carries Buzz's own Node plugins (plugins/) and the Node that
# runs them, pinned below and checked against its published checksum.
set -euo pipefail
node_version=22.23.3
case "$(uname -m)" in
  x86_64) node_arch=x64 node_sha256=df450af89261115ef9f9e3830c3eeb2cc9213b63c720b1af623cb5dcbe2e02de ;;
  aarch64) node_arch=arm64 node_sha256=a44aeb94849a299b22df10b9e622ec2f605c2183501bc40590705131de7c740f ;;
  *) echo "No Node for $(uname -m)." >&2; exit 1 ;;
esac
image=ubuntu:24.04
here="$(cd "$(dirname "$0")/.." && pwd)"
repo="$(cd "$here/../.." && pwd)"
cache="$HOME/.cache/buzz-gpui-deb"
# What one Ubuntu release builds links against its libraries, cargo-deb
# included; keep each release's apart.
cargo_home="$cache/cargo-${image//:/-}"
target="$cache/target-${image//:/-}"
mkdir -p "$here/dist" "$cargo_home" "$cache/rustup" "$target"
# Never leave an older package around to be mistaken for this build.
rm -f "$here"/dist/*.deb "$here"/dist/*.AppImage

docker run --rm \
  -v "$repo:$repo" \
  -v "$cargo_home:/root/.cargo" \
  -v "$cache/rustup:/root/.rustup" \
  -v "$target:$here/target-deb" \
  -e CARGO_TARGET_DIR="$here/target-deb" \
  -e OWNER="$(id -u):$(id -g)" \
  -e NODE_VERSION="$node_version" \
  -e NODE_ARCH="$node_arch" \
  -e NODE_SHA256="$node_sha256" \
  -e BUZZ_GPUI_RELEASE_VERSION \
  -e BUZZ_GPUI_VOLTAGE \
  -e BUZZ_GPUI_APPIMAGE \
  -w "$here" \
  "$image" bash -euo pipefail -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      ca-certificates curl build-essential pkg-config cmake clang \
      libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev libx11-dev \
      libx11-xcb-dev libvulkan-dev libfontconfig-dev libfreetype-dev libdbus-1-dev \
      libssl-dev libzstd-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
      xz-utils rsync >/dev/null
    # Node, for the plugins and to build them.
    bundle="$CARGO_TARGET_DIR/release/bundle"
    rm -rf "$bundle" && mkdir -p "$bundle/node/bin" "$bundle/plugins"
    tarball="/tmp/node-v$NODE_VERSION-linux-$NODE_ARCH.tar.xz"
    curl -sSfL -o "$tarball" "https://nodejs.org/dist/v$NODE_VERSION/node-v$NODE_VERSION-linux-$NODE_ARCH.tar.xz"
    echo "$NODE_SHA256  $tarball" | sha256sum -c --quiet
    mkdir -p /opt/node && tar -xJf "$tarball" -C /opt/node --strip-components=1
    cp /opt/node/bin/node "$bundle/node/bin/node"
    cp /opt/node/LICENSE "$bundle/node/LICENSE"
    # Build the plugins in a copy, so the checkout is left as it was.
    rsync -a --exclude node_modules --exclude dist plugins/ /tmp/plugins/
    (cd /tmp/plugins && PATH=/opt/node/bin:$PATH npm ci --no-audit --no-fund --loglevel=error \
      && PATH=/opt/node/bin:$PATH npm run build)
    for manifest in /tmp/plugins/*/buzz-plugin.json; do
      folder=$(dirname "$manifest")
      mkdir -p "$bundle/plugins/$(basename "$folder")"
      cp -r "$manifest" "$folder/dist" "$bundle/plugins/$(basename "$folder")/"
    done
    # The bundled Node runs every bundled plugin.
    /opt/node/bin/node scripts/check-plugins.mjs "$bundle/plugins"
    # Toolchains live in the mounted rustup cache; install one if it is empty.
    if [ ! -x /root/.cargo/bin/rustup ]; then
      curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --no-modify-path
    fi
    . /root/.cargo/env
    # The repository root pins the relay toolchain; GPUI uses its own pin.
    rustup toolchain install 1.98.1 --profile minimal --no-self-update
    export RUSTUP_TOOLCHAIN=1.98.1
    command -v cargo-deb >/dev/null || cargo install -q --locked cargo-deb
    # cargo-deb maps the target/release asset paths onto CARGO_TARGET_DIR.
    bash scripts/build-package.sh
    variant=()
    if [ "${BUZZ_GPUI_VOLTAGE:-0}" = 1 ]; then variant=(--variant voltage); fi
    cargo deb --no-build -p buzz-gpui "${variant[@]}" -o dist/ \
      ${BUZZ_GPUI_RELEASE_VERSION:+--deb-version "$BUZZ_GPUI_RELEASE_VERSION"}
    if [ "${BUZZ_GPUI_VOLTAGE:-0}" = 1 ]; then
      extracted=$(mktemp -d)
      dpkg-deb -x dist/*.deb "$extracted"
      "$bundle/node/bin/node" scripts/check-remote-provider.mjs "$extracted/usr/bin/buzz-backend-relay"
      rm -rf "$extracted"
    fi
    if [ "${BUZZ_GPUI_APPIMAGE:-0}" = 1 ]; then
      bash scripts/build-appimage.sh "${BUZZ_GPUI_RELEASE_VERSION:-$(dpkg-deb -f dist/*.deb Version)}"
    fi
    chown -R "$OWNER" dist
  '
ls -la "$here"/dist/
