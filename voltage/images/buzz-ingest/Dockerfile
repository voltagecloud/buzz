# syntax=docker/dockerfile:1.7
FROM rust:1.95-alpine3.22@sha256:064dfc925d68d1a63f4fd2871bd7dc6e6ea56692989a487185855d62885d90aa AS builder
RUN apk add --no-cache build-base cmake git musl-dev perl pkgconf
WORKDIR /build
ENV CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --locked --release -p buzz-ingest \
    && strip target/release/buzz-ingest

FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab
COPY --from=builder /build/target/release/buzz-ingest /usr/local/bin/buzz-ingest
USER 65532:65532
EXPOSE 5010
ENTRYPOINT ["/usr/local/bin/buzz-ingest"]
