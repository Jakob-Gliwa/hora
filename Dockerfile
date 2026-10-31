# syntax=docker/dockerfile:1
# Local builds compile from source; CI supplies its natively built, cached binary.
# Both produce a fully static musl binary (Alpine's native Rust target).
ARG BUILD_MODE=source
FROM rust:1.99.1-alpine AS build-source
RUN apk add --no-cache musl-dev linux-headers cmake make perl clang clang-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ARG TARGETARCH
RUN --mount=type=cache,id=hora-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=hora-musl-${TARGETARCH},target=/build/target,sharing=locked \
    cargo build --release --locked -p hora && cp target/release/hora /hora

FROM scratch AS build-prebuilt
COPY hora /hora

FROM build-${BUILD_MODE} AS binary

# Runtime: Alpine + CA certificates; busybox wget serves the healthcheck.
FROM alpine:3.24 AS runtime
RUN apk add --no-cache ca-certificates \
    && addgroup -S -g 10001 hora \
    && adduser -S -D -H -G hora -u 10001 hora \
    && mkdir -p /data && chown hora:hora /data
COPY --from=binary /hora /usr/local/bin/hora
ENV HORA_CONFIG=/etc/hora/config.toml \
    HORA_DATABASE_PATH=/data/hora.db \
    HORA_BIND=0.0.0.0:8787 \
    TOKIO_WORKER_THREADS=2
VOLUME ["/data"]
EXPOSE 8787
USER 10001:10001
HEALTHCHECK --interval=30s --timeout=3s --start-period=30s \
    CMD wget -qO /dev/null http://127.0.0.1:8787/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/hora"]
