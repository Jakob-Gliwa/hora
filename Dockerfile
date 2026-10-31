# syntax=docker/dockerfile:1
# Local builds compile from source; CI supplies its natively built, cached binary.
ARG BUILD_MODE=source
FROM rust:1.99.1-slim AS build-source
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ARG TARGETARCH
RUN --mount=type=cache,id=hora-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=hora-glibc-${TARGETARCH},target=/build/target,sharing=locked \
    cargo build --release --locked -p hora && cp target/release/hora /hora

FROM scratch AS build-prebuilt
COPY hora /hora

FROM build-${BUILD_MODE} AS binary

FROM debian:trixie-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 hora \
    && useradd --uid 10001 --gid 10001 --no-create-home hora \
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
