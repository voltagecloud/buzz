# syntax=docker/dockerfile:1.7
# Build from the repository root so the image packages the checked-out Buzz
# source; Dockerfile.dockerignore next to this file selects the context.
FROM rust:1.95-alpine3.22@sha256:064dfc925d68d1a63f4fd2871bd7dc6e6ea56692989a487185855d62885d90aa AS source
RUN apk add --no-cache bash build-base cmake git musl-dev openssl-dev \
    openssl-libs-static perl pkgconf protoc python3 procps
WORKDIR /build
ENV CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
COPY . .
FROM source AS package-checks
RUN apk add --no-cache tini \
    && tini -s -- cargo test --locked -p sprig -p buzz-acp -p buzz-agent -p buzz-dev-mcp -- --test-threads=1

FROM source AS builder
RUN cargo build --locked --profile sprig -p sprig \
    && strip target/sprig/sprig

FROM rust:1.97.1-slim-trixie@sha256:8e8cf8f7fd54a2d23d5a743b3a03f56e26b6c774276c33fa0595111704ebb15c AS development-rust

FROM node:22-trixie-slim@sha256:154ba2f4d6fec323d28e4f4bb86bba4677f1223391a1979cf521304e03a98dfa
LABEL maintainer="Voltage <hello@voltage.cloud>"
RUN apt-get update \
    && apt-get install -y --no-install-recommends bash ca-certificates curl git gh gnupg openssh-client socat tini \
        build-essential clang cmake pkg-config libssl-dev protobuf-compiler \
        python3 python3-venv python3-pip golang-go postgresql-client sqlite3 \
        jq unzip time procps just awscli ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 agent \
    && useradd --uid 10001 --gid 10001 --create-home --home-dir /home/agent agent
COPY --from=development-rust /usr/local/rustup /opt/rustup
COPY --from=development-rust /usr/local/cargo/bin /usr/local/bin
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/home/agent/.cargo CARGO_BUILD_JOBS=2
# Release assets are public and pinned by checksum per architecture;
# credentials enter only at runtime.
ARG TARGETARCH
RUN case "$TARGETARCH" in \
        amd64) via=x86_64 gitleaks=x64 sccache=x86_64 \
            kubectl_sha=874d5e72dbb819f43cff16bcd1e4f8bac5b7f2361fe1e55049b0a6c676fb0cbf \
            via_sha=9e5a7f9384cacda29061d814e9397e7b6d90a72ee4663775362978ac9b65d8b7 \
            op_sha=198b05dcf9a0972778ce5a4e262c459979b0c837257b5da65e2fba6187734226 \
            gitleaks_sha=551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb \
            depot_sha=7a469c767bd3b7e67b3174f91af9c6315fea0ced4871948ee9d41a006fb2347e \
            sccache_sha=45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89 \
            ghstack_sha=957e822cfa7e435cd84a297cd96c419b58e51e3ac58be249762c24bd9e45099b ;; \
        arm64) via=arm64 gitleaks=arm64 sccache=aarch64 \
            kubectl_sha=cc749967b62f4422260bc9c0aa7a7c55f45175ae38cb8d95767b5d2b7e04c1fd \
            via_sha=26cc174802c875424b9846a398d4b2d890b911506f7c574dee96859620caa535 \
            op_sha=7b5c361a23e738a3969cf676960f4843af4aa9aa68dbec23d18a9134da428ba3 \
            gitleaks_sha=e4a487ee7ccd7d3a7f7ec08657610aa3606637dab924210b3aee62570fb4b080 \
            depot_sha=042d846f544919e6b5bcb328387b1963af4e298bfa441a41f36bc46663202f2b \
            sccache_sha=2b3284d5da3b46a47dc4229e75bb7b88ac4aa99c8d754fb7d2f84997e5a4354a \
            ghstack_sha=803ec91eeba9b2ac38827e299a79830fe75ca7284f0c28d60e6a2704bb34ed13 ;; \
        *) echo "Unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl -fsSL "https://github.com/tee8z/via/releases/download/v0.6.1/via-linux-$via.tar.gz" -o /tmp/via.tar.gz \
    && echo "$via_sha  /tmp/via.tar.gz" | sha256sum -c - \
    && mkdir -p /opt/agent-tools \
    && tar -xzf /tmp/via.tar.gz -C /opt/agent-tools via \
    && curl -fsSL "https://cache.agilebits.com/dist/1P/op2/pkg/v2.34.0/op_linux_${TARGETARCH}_v2.34.0.zip" -o /tmp/op.zip \
    && echo "$op_sha  /tmp/op.zip" | sha256sum -c - \
    && unzip -p /tmp/op.zip op > /usr/local/bin/op \
    && chmod 0755 /usr/local/bin/op \
    && curl -fsSL "https://github.com/gitleaks/gitleaks/releases/download/v8.30.1/gitleaks_8.30.1_linux_$gitleaks.tar.gz" -o /tmp/gitleaks.tar.gz \
    && echo "$gitleaks_sha  /tmp/gitleaks.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/gitleaks.tar.gz -C /usr/local/bin gitleaks \
    && curl -fsSL "https://github.com/depot/cli/releases/download/v2.102.18/depot_2.102.18_linux_${TARGETARCH}.tar.gz" -o /tmp/depot.tar.gz \
    && echo "$depot_sha  /tmp/depot.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/depot.tar.gz -C /usr/local/bin --strip-components=1 bin/depot \
    && curl -fsSL "https://github.com/mozilla/sccache/releases/download/v0.18.0/sccache-v0.18.0-$sccache-unknown-linux-musl.tar.gz" -o /tmp/sccache.tar.gz \
    && echo "$sccache_sha  /tmp/sccache.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/sccache.tar.gz -C /usr/local/bin --strip-components=1 "sccache-v0.18.0-$sccache-unknown-linux-musl/sccache" \
    && curl -fsSL "https://dl.k8s.io/release/v1.35.8/bin/linux/${TARGETARCH}/kubectl" -o /usr/local/bin/kubectl \
    && echo "$kubectl_sha  /usr/local/bin/kubectl" | sha256sum -c - \
    && chmod 0755 /usr/local/bin/kubectl \
    && mkdir -p /opt/gh-extensions/gh-stack \
    && curl -fsSL "https://github.com/github/gh-stack/releases/download/v0.3.0/linux-${TARGETARCH}" -o /opt/gh-extensions/gh-stack/gh-stack \
    && echo "$ghstack_sha  /opt/gh-extensions/gh-stack/gh-stack" | sha256sum -c - \
    && chmod 0755 /opt/gh-extensions/gh-stack/gh-stack \
    && printf 'owner: github\nname: gh-stack\nhost: github.com\ntag: v0.3.0\nispinned: true\npath: /opt/gh-extensions/gh-stack/gh-stack\n' > /opt/gh-extensions/gh-stack/manifest.yml \
    && rm /tmp/via.tar.gz /tmp/op.zip /tmp/gitleaks.tar.gz /tmp/depot.tar.gz /tmp/sccache.tar.gz
# Browser recording and narration for the walkthrough-video skill.
# Each Playwright version has its own browser build, so the image carries only
# the system libraries Chromium needs; the agent installs the browser that
# matches the target repository's Playwright at runtime. Piper and its voices
# live outside the /home/agent volume. The skill's setup script finds them
# through WALKTHROUGH_CACHE and skips its own setup.
ENV WALKTHROUGH_CACHE=/opt/walkthrough-video
RUN npx --yes playwright@1.64.1 install-deps chromium \
    && rm -rf /var/lib/apt/lists/* /root/.npm \
    && python3 -m venv "$WALKTHROUGH_CACHE/venv" \
    && "$WALKTHROUGH_CACHE/venv/bin/pip" install --no-cache-dir piper-tts==1.8.0 \
    && mkdir -p "$WALKTHROUGH_CACHE/voices" \
    && cd "$WALKTHROUGH_CACHE/voices" \
    && voices=https://huggingface.co/rhasspy/piper-voices/resolve/e7229f7d33a10e145e109b886d661984201ac3c4/en \
    && for voice in en_GB/alan/medium/en_GB-alan-medium en_US/lessac/medium/en_US-lessac-medium; do \
        curl -fsSLO "$voices/$voice.onnx" && curl -fsSLO "$voices/$voice.onnx.json"; \
    done \
    && printf '%s  %s\n' \
        0a309668932205e762801f1efc2736cd4b0120329622adf62be09e56339d3330 en_GB-alan-medium.onnx \
        c0f0d124e5895c00e7c03b35dcc8287f319a6998a365b182deb5c8e752ee8c1e en_GB-alan-medium.onnx.json \
        5efe09e69902187827af646e1a6e9d269dee769f9877d17b16b1b46eeaaf019f en_US-lessac-medium.onnx \
        efe19c417bed055f2d69908248c6ba650fa135bc868b0e6abb3da181dab690a0 en_US-lessac-medium.onnx.json \
        | sha256sum -c -
COPY voltage/images/buzz-agent/checkpoint-gitleaks.toml /opt/agent-tools/checkpoint-gitleaks.toml
WORKDIR /opt/buzz-runtimes
COPY voltage/images/buzz-agent/package.json voltage/images/buzz-agent/package-lock.json ./
RUN npm ci --omit=dev --no-audit --no-fund \
    # This image is Debian (glibc), not Alpine (musl).
    && rm -rf node_modules/@anthropic-ai/*-linux-*-musl \
    && npm cache clean --force
COPY --from=builder /build/target/sprig/sprig /usr/local/bin/sprig
COPY --chmod=0755 voltage/images/buzz-agent/account.py /usr/local/bin/buzz-agent-account
COPY --chmod=0755 voltage/images/buzz-agent/profile.py /usr/local/bin/buzz-agent-profile
COPY --chmod=0755 voltage/images/buzz-agent/cache.py /usr/local/bin/buzz-agent-cache
COPY --chmod=0755 voltage/images/buzz-agent/checkpoint.py /usr/local/bin/buzz-agent-checkpoint
COPY --chmod=0755 voltage/images/buzz-agent/review.py /usr/local/bin/buzz-agent-review
COPY --chmod=0755 voltage/images/buzz-agent/heavy.py /usr/local/bin/buzz-heavy
COPY --chmod=0755 voltage/images/buzz-agent/worker.py /usr/local/bin/buzz-agent-worker
COPY --chmod=0755 voltage/images/buzz-agent/subagent.py /usr/local/bin/buzz-subagent
# COPY --chmod also sets the mode of a directory it creates, so create this one first:
# a 0644 /etc/buzz-agent cannot be entered, and every Sandbox with sub-agents failed to start.
RUN install -d -m 0755 /etc/buzz-agent
COPY --chmod=0644 voltage/images/buzz-agent/subagents.md /etc/buzz-agent/subagents.md
COPY --chmod=0644 voltage/images/buzz-agent/cluster.md /etc/buzz-agent/cluster.md
# Voltage engineering skills from the `engineering-skills` build context, made by
# select-skills.sh: master of voltagecloud/engineering-skills on main, empty for
# pull requests and local builds. Root owns the copy, so the agent can read but
# not change it; profile.py links each skill into the harnesses.
# hadolint ignore=DL3022
COPY --from=engineering-skills . /opt/engineering-skills/
COPY --chmod=0644 voltage/images/buzz-agent/environment.md /etc/buzz-agent/environment.md
COPY --chmod=0755 voltage/images/buzz-agent/heavy-bin/ /opt/buzz-heavy/bin/
COPY --chmod=0755 voltage/images/buzz-agent/gh.sh /usr/local/bin/gh
COPY --chmod=0755 voltage/images/buzz-agent/via.sh /usr/local/bin/via
COPY --chmod=0755 voltage/images/buzz-agent/entrypoint.sh /usr/local/bin/sprig-entrypoint
RUN ln -s /usr/local/bin/buzz-heavy /usr/local/bin/buzz-agent-gate \
    && for name in buzz-acp buzz-agent buzz-dev-mcp rg tree buzz git-credential-nostr git-sign-nostr; do \
        ln -s sprig "/usr/local/bin/$name"; \
    done
# /opt/buzz-heavy/bin gates compiling cargo and go commands through buzz-heavy.
ENV HOME=/home/agent PATH=/opt/buzz-heavy/bin:/opt/buzz-runtimes/node_modules/.bin:/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin
WORKDIR /home/agent
USER 10001:10001
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/sprig-entrypoint"]
