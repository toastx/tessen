# syntax=docker/dockerfile:1.7
FROM rust:1.89-slim-bookworm AS builder
ARG TARGETARCH=amd64

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates clang cmake make musl-tools perl pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
ENV CC_x86_64_unknown_linux_musl=musl-gcc \
    CC_aarch64_unknown_linux_musl=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc
COPY Cargo.toml ./Cargo.toml
COPY programs/tessen/Cargo.toml programs/tessen/Cargo.toml
COPY programs/tessen/src programs/tessen/src
COPY keeper/Cargo.toml keeper/Cargo.lock keeper/
COPY keeper/src keeper/src

RUN case "$TARGETARCH" in \
        amd64) rust_target="x86_64-unknown-linux-musl" ;; \
        arm64) rust_target="aarch64-unknown-linux-musl" ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && rustup target add "$rust_target" \
    && cargo build --locked --release --target "$rust_target" --manifest-path keeper/Cargo.toml \
    && mkdir /out \
    && cp "keeper/target/$rust_target/release/tessen-keeper" /out/tessen-keeper

FROM alpine:3.22 AS runtime
RUN apk add --no-cache ca-certificates libgcc \
    && addgroup -S tessen \
    && adduser -S -G tessen tessen

USER tessen
WORKDIR /app
COPY --from=builder /out/tessen-keeper /usr/local/bin/tessen-keeper

ENTRYPOINT ["/usr/local/bin/tessen-keeper"]
