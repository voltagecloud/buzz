# syntax=docker/dockerfile:1.7
# Headless Paseo daemon (paseo.sh) that runs one developer's personal
# assistant (PA) in front of their Buzz agents. The daemon is the unmodified
# upstream image. This image adds the `buzz` CLI, the native assistant runtimes, the PA
# instructions, and a wrapper that writes Paseo's config from the environment.
# See README.md next to this file.
# Build from the repository root; Dockerfile.dockerignore next to this file
# selects the context.
# Build via v0.6.1 from immutable, checksum-pinned source. Its published
# GNU/Linux binaries need glibc 2.39, newer than the daemon runtime.
# Native Alpine builds produce static musl binaries on both image architectures.
FROM rust:1.95-alpine3.22@sha256:064dfc925d68d1a63f4fd2871bd7dc6e6ea56692989a487185855d62885d90aa AS via-builder
RUN apk add --no-cache build-base curl musl-dev
WORKDIR /via
ENV CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
RUN curl -fsSL https://codeload.github.com/tee8z/via/tar.gz/87d1301158d9c1ac585110869f8bb993b15270da -o /tmp/via.tar.gz \
    && echo "7dcd493f5db9a7406bac7448a52f97de7a03bf7128ceff390910dfae8b525428  /tmp/via.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/via.tar.gz --strip-components=1 \
    && cargo build --locked --release --bin via \
    && install -D -m 0755 -s target/release/via /out/via \
    && readelf -l /out/via > /tmp/via-headers \
    && if grep -q INTERP /tmp/via-headers; then echo "via must be statically linked" >&2; exit 1; fi \
    && /out/via version | grep -qx 'via 0.6.1' \
    && rm -rf /tmp/via.tar.gz /tmp/via-headers target

FROM rust:1.95-alpine3.22@sha256:064dfc925d68d1a63f4fd2871bd7dc6e6ea56692989a487185855d62885d90aa AS builder
RUN apk add --no-cache build-base cmake git musl-dev perl pkgconf
WORKDIR /build
ENV CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
# Only the Cargo workspace: a change to the wrapper or the docs does not
# rebuild the CLI.
COPY .cargo .cargo
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
COPY examples examples
COPY scripts scripts
COPY test-fixtures test-fixtures
# The musl target links statically, so the binary runs on the Debian base.
# The target directory is removed in the same layer to keep the cache small.
RUN cargo build --locked --release -p buzz-cli --bin buzz \
    && install -D -m 0755 -s target/release/buzz /out/buzz \
    && rm -rf target

# Use the developer image's integrity-locked Claude Code and Codex releases.
# Copy native executables and Codex resources, without package tools or adapters.
FROM node:22-trixie-slim@sha256:154ba2f4d6fec323d28e4f4bb86bba4677f1223391a1979cf521304e03a98dfa AS assistant-runtimes
WORKDIR /opt/buzz-runtimes
COPY voltage/images/buzz-agent/package.json voltage/images/buzz-agent/package-lock.json ./
COPY voltage/images/paseo-daemon/seccomp.mjs /tmp/seccomp.mjs
RUN npm ci --omit=dev --no-audit --no-fund \
    && install -D -m 0755 node_modules/@anthropic-ai/claude-code/bin/claude.exe /out/claude \
    && mkdir -p /out/codex \
    && case "$(node -p process.arch)" in x64) platform=x64; triple=x86_64 ;; arm64) platform=arm64; triple=aarch64 ;; *) exit 1 ;; esac \
    && cp -a "node_modules/@openai/codex-linux-${platform}/vendor/${triple}-unknown-linux-musl/." /out/codex/ \
    && node /tmp/seccomp.mjs /out/seccomp

# ghcr.io/getpaseo/paseo:0.15.3 (linux/amd64 and linux/arm64)
FROM ghcr.io/getpaseo/paseo@sha256:57048c43a2c2988434273c7f8a4b25aa585bca6c1720493ae52e2fa91497b99a
LABEL maintainer="Voltage <hello@voltage.cloud>"
USER root
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
# Paseo's Claude provider resolves the executable from PATH.
COPY --from=assistant-runtimes /out/claude /usr/local/bin/claude
COPY --from=assistant-runtimes /out/codex /opt/paseo-buzz/codex
COPY --from=assistant-runtimes /out/seccomp/ /etc/paseo-buzz/
# COPY --chmod also sets newly created parent directories to that mode.
# Create this directory first so UID 1000 can traverse it.
RUN install -d -m 0755 /etc/codex
COPY --chmod=0644 voltage/images/paseo-daemon/codex-requirements.toml /etc/codex/requirements.toml
COPY --chmod=0644 voltage/images/paseo-daemon/codex-config.toml /etc/codex/config.toml
RUN ln -s /opt/paseo-buzz/codex/bin/codex /usr/local/bin/codex \
    && ln -s /opt/paseo-buzz/codex/codex-resources/bwrap /usr/local/bin/bwrap \
    && HOME=/tmp claude --version | grep -qx '2.1.325 (Claude Code)' \
    && HOME=/tmp codex --version | grep -qx 'codex-cli 0.169.0'
# Install analysis tools from Debian before removing package managers.
# Debian's yq is the Python jq wrapper, not mikefarah/yq. Python is for
# analysis only; no pip, compilers, build/test tools, or scanners are added.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        jq python3 sqlite3 xxd file tree unzip zstd less fd-find yq bsdextrautils \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/corepack \
    && rm -f /usr/local/bin/{npm,npx,corepack,yarn,yarnpkg} \
    && rm -rf /var/lib/apt/lists /usr/lib/apt /root/.npm \
    && rm -f /usr/bin/apt* /usr/bin/dpkg* /usr/bin/cpan* \
    && install -d -o root -g root -m 0555 /opt/paseo-buzz/no-terminal-worker /opt/paseo-buzz/no-hooks /etc/paseo-buzz /etc/paseo-buzz/agents /usr/local/lib/paseo-buzz
COPY --from=builder /out/buzz /usr/local/bin/buzz
COPY --from=via-builder /out/via /opt/agent-tools/via
COPY --chmod=0755 voltage/images/paseo-daemon/via.sh /usr/local/bin/via
COPY voltage/images/paseo-daemon/recorder /opt/paseo-buzz/recorder
COPY voltage/images/paseo-daemon/profile /opt/paseo-buzz/profile
COPY --chmod=0644 voltage/images/paseo-daemon/assistant.md /etc/paseo-buzz/assistant.md
COPY --chmod=0755 voltage/images/paseo-daemon/entrypoint.sh /usr/local/bin/paseo-buzz-entrypoint
COPY --chmod=0755 voltage/images/paseo-daemon/code.mjs /usr/local/bin/paseo-code
COPY --chmod=0755 voltage/images/paseo-daemon/workspaces.mjs /usr/local/bin/paseo-workspaces
COPY --chmod=0644 voltage/images/paseo-daemon/providers.mjs voltage/images/paseo-daemon/agents-watch.mjs voltage/images/paseo-daemon/restore.mjs voltage/images/paseo-daemon/tools.mjs /usr/local/lib/paseo-buzz/
# Relay-only and TLS-only. The listener is loopback, for the in-container
# CLI, the agent MCP server, and the health check. Clients, including the web
# app at app.paseo.sh, reach the daemon only through the relay. The daemon's
# own web UI and the speech models are off (dictation downloads about 631 MB
# at first start). The wrapper refuses to start without TLS. Claude Code does
# not update itself or send telemetry.
ENV PASEO_LISTEN=127.0.0.1:6767 \
    PASEO_RELAY_ENABLED=true \
    PASEO_RELAY_ENDPOINT=buzz.voltage.cloud:443 \
    PASEO_RELAY_USE_TLS=true \
    PASEO_RELAY_PUBLIC_USE_TLS=true \
    PASEO_WEB_UI_ENABLED=false \
    PASEO_DICTATION_ENABLED=false \
    BUZZ_RELAY_URL=wss://buzz.voltage.cloud \
    DISABLE_AUTOUPDATER=1 \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
USER 1000:1000
WORKDIR /home/paseo
ENTRYPOINT ["/usr/local/bin/paseo-buzz-entrypoint"]
