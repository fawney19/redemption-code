# AetherPool

AetherPool is a standalone Codex/OpenAI OAuth account pool service extracted from Aether-style pool operations.

## Layout

- `apps/api`: Rust + Axum API service.
- `apps/web`: Vue 3 + Vite admin/user console.
- `crates/account-pool-core`: Codex auth parsing, export formats, token expiry rules, wham/usage normalization, redeem code helpers.
- `crates/account-pool-data`: SQLite schema, repositories, encrypted auth storage, redeem allocation.

## Behavior

- Imported Codex accounts start as `available`, or `at_expired` when the access token is already expired.
- Redeem exports use exclusive allocation. Redeemed accounts stay in the database with `status = redeemed`, `redeemed_at`, `redeem_code_id`, and `redemption_id`.
- Redeemed accounts are not returned to the allocatable pool, are skipped by refresh/probe loading paths, and keep a redemption-time auth snapshot for repeat downloads.
- Access tokens are refreshed only for unredeemed accounts. Refresh failures mark accounts as `refresh_failed`.
- Automatic health probing can be enabled from the admin console. It uses the same unredeemed account loading path, supports interval/concurrency/batch-size settings, and can refresh unredeemed access tokens before probing.
- Admin exports default to unredeemed accounts. CPA single-account export returns one auth JSON object; CPA multi-account export downloads a ZIP package with one auth JSON file per account. Sub2API export returns `{ exported_at, proxies, accounts }`.
- Redeem-code batch status exports from the admin console are CSV files. Full plaintext redeem codes are only shown/exported at generation time.

## API

- `GET /health`
- `POST /api/admin/accounts/import`
- `GET /api/admin/accounts`
- `POST /api/admin/accounts/probe`
- `POST /api/admin/accounts/refresh`
- `POST /api/admin/accounts/export`
- `GET /api/admin/settings/auto-probe`
- `POST /api/admin/settings/auto-probe`
- `POST /api/admin/settings/auto-probe/run`
- `POST /api/admin/redeem-code-batches`
- `GET /api/admin/redeem-code-batches`
- `GET /api/admin/redeem-code-batches/{batch_id}/codes`
- `POST /api/redeem/export`

Admin endpoints accept `Authorization: Bearer <AETHER_POOL_ADMIN_TOKEN>` or `x-admin-token`.
By default admin endpoints are locked when `AETHER_POOL_ADMIN_TOKEN` is empty. Use `AETHER_POOL_ALLOW_OPEN_ADMIN=1` only for isolated local development.
Cross-origin browser access is restricted by `AETHER_POOL_CORS_ORIGINS`, defaulting to local Vite origins.

The public web entry is `/` and only shows the redeem/export page. The management console is available at `/admin` and is not linked from the public page.

## Development

```bash
cd aether-pool
export AETHER_POOL_ADMIN_TOKEN=dev-admin-token
export AETHER_POOL_SECRET_KEY=change-me-before-production
cargo run -p aether-pool-api
```

The API defaults to `127.0.0.1:8318` and SQLite at `data/aether-pool.sqlite3`.

Frontend:

```bash
cd aether-pool/apps/web
npm install
VITE_API_BASE=http://127.0.0.1:8318 npm run dev -- --port 5178
```

Set `VITE_API_BASE=http://127.0.0.1:8318` when the API runs on a different origin.

## Server deployment with Baota Nginx

The included Compose file does not run an Nginx container. It builds/runs the API container and provides a one-shot frontend build service. Let Baota Nginx serve `deploy/web` and reverse proxy `/api` plus `/health` to the API.

```bash
cd aether-pool
cp .env.example .env
vim .env

# Build frontend assets into ./deploy/web for Baota's site root.
docker compose --profile build run --rm web-build

# Build and start the API on 127.0.0.1:8318 by default.
docker compose up -d --build api
docker compose ps
```

Set the Baota site root to:

```text
/absolute/path/to/aether-pool/deploy/web
```

Add these location rules in the Baota Nginx site config:

```nginx
location / {
  try_files $uri $uri/ /index.html;
}

location /api/ {
  proxy_pass http://127.0.0.1:8318;
  proxy_set_header Host $host;
  proxy_set_header X-Real-IP $remote_addr;
  proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
  proxy_set_header X-Forwarded-Proto $scheme;
}

location = /health {
  proxy_pass http://127.0.0.1:8318/health;
  proxy_set_header Host $host;
  proxy_set_header X-Real-IP $remote_addr;
  proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
  proxy_set_header X-Forwarded-Proto $scheme;
}
```

For same-domain deployment, keep `VITE_API_BASE=` empty in `.env` so the frontend calls `/api/...`. If API and frontend use different domains, set `VITE_API_BASE` to the API origin and set `AETHER_POOL_CORS_ORIGINS` to the frontend origin.

Update deployment after pulling new code:

```bash
cd aether-pool
docker compose --profile build run --rm web-build
docker compose up -d --build api
```

## Verification

```bash
cd aether-pool
cargo test
cargo fmt --all -- --check
RUSTC_WRAPPER= cargo clippy --all-targets --all-features -- -D warnings

cd apps/web
npm run build
```
