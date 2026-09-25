# syntax=docker/dockerfile:1.7
ARG TARGETPLATFORM

FROM --platform=$TARGETPLATFORM rust:1.89-slim-bookworm AS builder
ARG TARGETARCH

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
COPY backend/Cargo.toml backend/Cargo.lock backend/
COPY backend/src backend/src

RUN case "$TARGETARCH" in \
        amd64) rust_target="x86_64-unknown-linux-musl" ;; \
        arm64) rust_target="aarch64-unknown-linux-musl" ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && rustup target add "$rust_target" \
    && cargo build --locked --release --target "$rust_target" --manifest-path backend/Cargo.toml \
    && mkdir /out \
    && cp "backend/target/$rust_target/release/tessen-backend" /out/tessen-backend

FROM --platform=$TARGETPLATFORM alpine:3.22 AS runtime
RUN apk add --no-cache ca-certificates libgcc \
    && addgroup -S tessen \
    && adduser -S -G tessen tessen \
    && mkdir /data \
    && chown tessen:tessen /data

USER tessen
WORKDIR /data
COPY --from=builder /out/tessen-backend /usr/local/bin/tessen-backend

ENV DB_PATH=/data/tessen.db
EXPOSE 8080
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8080/health >/dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/tessen-backend"]
