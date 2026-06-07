# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY backend/Cargo.toml backend/Cargo.toml
COPY backend backend

RUN cargo build --release -p aether-pool-api

FROM node:22-alpine AS web-builder

WORKDIR /app/frontend
ARG VITE_API_BASE=
ENV VITE_API_BASE=${VITE_API_BASE}

COPY frontend/package*.json ./
RUN npm ci

COPY frontend/ ./
RUN npm run build

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl libsqlite3-0 \
  && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --gid users aether \
  && mkdir -p /data \
  && chown -R aether:users /data

COPY --from=builder /app/target/release/aether-pool-api /usr/local/bin/aether-pool-api
COPY --from=web-builder /app/frontend/dist /usr/local/share/aether-pool/web

USER root
WORKDIR /data

ENV AETHER_POOL_ADDR=0.0.0.0:8318
ENV AETHER_POOL_DATABASE_URL=sqlite:///data/aether-pool.sqlite3
ENV AETHER_POOL_WEB_DIR=/usr/local/share/aether-pool/web
ENV RUST_LOG=aether_pool_api=info,tower_http=info

EXPOSE 8318

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8318/health >/dev/null || exit 1

CMD ["aether-pool-api"]
