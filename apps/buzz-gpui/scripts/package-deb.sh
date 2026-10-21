#!/usr/bin/env bash
# Builds the Ubuntu .deb inside an ubuntu:26.04 container, so the binary links
# against Ubuntu's libraries rather than Nix store paths.
#
# Usage: scripts/package-deb.sh            -> dist/buzz-gpui_<version>_amd64.deb
#
# The package carries Buzz's own Node plugins (plugins/) and the Node that
# runs them, pinned below and checked against its published checksum.
set -euo pipefail
node_version=22.23.3
node_sha256=df450af89261115ef9f9e3830c3eeb2cc9213b63c720b1af623cb5dcbe2e02de
here="$(cd "$(dirname "$0")/.." && pwd)"
cache="$HOME/.cache/buzz-gpui-deb"
mkdir -p "$here/dist" "$cache/cargo" "$cache/rustup" "$cache/target"
# Never leave an older package around to be mistaken for this build.
rm -f "$here"/dist/*.deb

docker run --rm \
  -v "$here:$here" \
  -v "$cache/cargo:/root/.cargo" \
  -v "$cache/rustup:/root/.rustup" \
  -v "$cache/target:$here/target-deb" \
  -e CARGO_TARGET_DIR="$here/target-deb" \
  -e OWNER="$(id -u):$(id -g)" \
  -e NODE_VERSION="$node_version" \
  -e NODE_SHA256="$node_sha256" \
  -w "$here" \
  ubuntu:26.04 bash -euo pipefail -c '
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
    tarball="/tmp/node-v$NODE_VERSION-linux-x64.tar.xz"
    curl -sSfL -o "$tarball" "https://nodejs.org/dist/v$NODE_VERSION/node-v$NODE_VERSION-linux-x64.tar.xz"
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
    rustup toolchain list | grep -q stable || rustup default stable
    command -v cargo-deb >/dev/null || cargo install -q --locked cargo-deb
    # cargo-deb maps the target/release asset paths onto CARGO_TARGET_DIR.
    cargo build --locked --release -p buzz-gpui
    cargo deb --no-build -p buzz-gpui -o dist/
    chown -R "$OWNER" dist
  '
ls -la "$here"/dist/*.deb
