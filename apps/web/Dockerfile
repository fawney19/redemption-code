# syntax=docker/dockerfile:1.7

FROM node:22-alpine AS builder

WORKDIR /app
ARG VITE_API_BASE=
ENV VITE_API_BASE=${VITE_API_BASE}

COPY apps/web/package*.json ./
RUN npm ci

COPY apps/web/ ./
RUN npm run build

FROM nginx:1.27-alpine AS runtime

COPY apps/web/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=builder /app/dist /usr/share/nginx/html

EXPOSE 80

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD wget -qO- http://127.0.0.1/web-health >/dev/null || exit 1
