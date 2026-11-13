# syntax=docker/dockerfile:1.7
# Headless Paseo daemon (paseo.sh) that runs one developer's personal
# assistant (PA) in front of their Buzz agents. The daemon is the unmodified
# upstream image. This image adds the `buzz` CLI, the Claude assistant runtime, the PA
# instructions, and a wrapper that writes Paseo's config from the environment.
# See README.md next to this file.
# Build from the repository root; Dockerfile.dockerignore next to this file
# selects the context.
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

# ghcr.io/getpaseo/paseo:0.15.3 (linux/amd64 and linux/arm64)
FROM ghcr.io/getpaseo/paseo@sha256:57048c43a2c2988434273c7f8a4b25aa585bca6c1720493ae52e2fa91497b99a
LABEL maintainer="Voltage <hello@voltage.cloud>"
USER root
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
# The Claude Code CLI is the binary that the daemon's Claude Agent SDK bundles
# for this platform (Claude Code 2.1.246 with SDK 0.3.246 in Paseo 0.15.3), so
# the base digest pins it and it matches the SDK. Paseo's claude provider
# runs `claude` from PATH.
RUN shopt -s nullglob \
    && binaries=(/usr/local/lib/node_modules/@getpaseo/server/node_modules/@anthropic-ai/claude-agent-sdk-linux-*/claude) \
    && if [ "${#binaries[@]}" -ne 1 ]; then echo "expected one bundled Claude Code binary, found: ${binaries[*]}" >&2; exit 1; fi \
    && ln -s "${binaries[0]}" /usr/local/bin/claude \
    && HOME=/tmp claude --version | grep -qx '2.1.246 (Claude Code)'
# The runtime carries no package managers, compilers, build/test tools, or
# scanners. Node is the upstream daemon runtime, not an installation surface.
RUN rm -rf /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/corepack \
    && rm -f /usr/local/bin/{npm,npx,corepack,yarn,yarnpkg} \
    && rm -rf /var/lib/apt/lists /usr/lib/apt /root/.npm \
    && rm -f /usr/bin/apt* /usr/bin/dpkg* /usr/bin/cpan* \
    && install -d -o root -g root -m 0555 /opt/paseo-buzz/no-terminal-worker /opt/paseo-buzz/no-hooks /etc/paseo-buzz /etc/paseo-buzz/agents /usr/local/lib/paseo-buzz
COPY --from=builder /out/buzz /usr/local/bin/buzz
COPY voltage/images/paseo-daemon/recorder /opt/paseo-buzz/recorder
COPY --chmod=0644 voltage/images/paseo-daemon/assistant.md /etc/paseo-buzz/assistant.md
COPY --chmod=0755 voltage/images/paseo-daemon/entrypoint.sh /usr/local/bin/paseo-buzz-entrypoint
COPY --chmod=0755 voltage/images/paseo-daemon/code.mjs /usr/local/bin/paseo-code
COPY --chmod=0755 voltage/images/paseo-daemon/workspaces.mjs /usr/local/bin/paseo-workspaces
COPY --chmod=0644 voltage/images/paseo-daemon/providers.mjs voltage/images/paseo-daemon/agents-watch.mjs /usr/local/lib/paseo-buzz/
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
