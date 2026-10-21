# syntax=docker/dockerfile:1.7
# In-cluster reaper for ended and orphaned Buzz Agent Sandbox sessions.
# Build from the repository root so the image packages the checked-out Buzz
# source; Dockerfile.dockerignore next to this file selects the context.
# .github/workflows/voltage-agent-images.yml publishes it from main to the
# voltage-buzz-sandbox-reaper-staging ECR repository.
FROM rust:1.95-alpine3.22@sha256:064dfc925d68d1a63f4fd2871bd7dc6e6ea56692989a487185855d62885d90aa AS builder
RUN apk add --no-cache build-base cmake git musl-dev perl pkgconf
WORKDIR /build
ENV CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
COPY . .
# The musl target links statically, so the runtime image needs no libc.
RUN cargo build --locked --release -p buzz-sandbox-reaper --bin buzz-sandbox-reaper \
    && strip target/release/buzz-sandbox-reaper

FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab
LABEL maintainer="Voltage <hello@voltage.cloud>"
COPY --from=builder /build/target/release/buzz-sandbox-reaper /usr/local/bin/buzz-sandbox-reaper
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/buzz-sandbox-reaper"]
CMD ["reconcile"]
