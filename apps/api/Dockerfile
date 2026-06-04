# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY apps/api/Cargo.toml apps/api/Cargo.toml
COPY crates/account-pool-core/Cargo.toml crates/account-pool-core/Cargo.toml
COPY crates/account-pool-data/Cargo.toml crates/account-pool-data/Cargo.toml
COPY apps apps
COPY crates crates

RUN cargo build --release -p aether-pool-api

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl libsqlite3-0 \
  && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --gid users aether \
  && mkdir -p /data \
  && chown -R aether:users /data

COPY --from=builder /app/target/release/aether-pool-api /usr/local/bin/aether-pool-api

USER aether
WORKDIR /data

ENV AETHER_POOL_ADDR=0.0.0.0:8318
ENV AETHER_POOL_DATABASE_URL=sqlite:///data/aether-pool.sqlite3
ENV RUST_LOG=aether_pool_api=info,tower_http=info

EXPOSE 8318

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8318/health >/dev/null || exit 1

CMD ["aether-pool-api"]
