# AetherPool

AetherPool is a standalone Codex/OpenAI OAuth account pool service extracted from Aether-style pool operations.

## Layout

- `apps/api`: Rust + Axum API service.
- `apps/web`: Vue 3 + Vite admin/user console.
- `crates/account-pool-core`: Codex auth parsing, export formats, token expiry rules, wham/usage normalization, redeem code helpers.
- `crates/account-pool-data`: SQLite schema, repositories, encrypted auth storage, redeem allocation.

## Behavior

- Imported Codex accounts start as `available`, or `at_expired` when the access token is already expired.
- Redeem exports use exclusive allocation. Redeemed accounts stay in the database with `redeemed_at`, `redeem_code_id`, and `redemption_id`; the account `status` remains the latest health state.
- Redeemed accounts are not returned to the allocatable pool, are skipped by automatic refresh/probe loading paths, and keep a redemption-time auth snapshot for repeat downloads.
- Access tokens are refreshed only for unredeemed accounts. Refresh failures mark accounts as `refresh_failed`.
- Automatic health probing can be enabled from the admin console. It uses the unredeemed account loading path, supports interval/concurrency/batch-size settings, supports fixed or dynamic proxy egress, and only calls the `wham/usage` quota endpoint. It does not refresh access tokens.
- Public redeem export pre-refreshes only the expired unredeemed accounts that may be needed by the submitted codes, then allocates only accounts whose access token expiry is outside the refresh grace window. It never refreshes redeemed accounts.
- Public redeem export is capped at 500 codes and 1,000 exported accounts per request to keep anonymous downloads bounded.
- Public after-sale reissue accepts already redeemed codes, probes the current bound accounts, and only reissues when every current account is in an auth-failure state: `at_expired`, `refresh_failed`, `auth_invalid`, or `forbidden`. `quota_exhausted` is not eligible for self-service reissue.
- After-sale reissue is limited per redeem-code batch by `after_sale_limit`; existing databases are upgraded with a default limit of `1` per code. A limit of `0` disables after-sale for that batch.
- After-sale reissue appends `redeem_after_sales` history and creates a new redemption snapshot for the replacement account. Original redeemed accounts and original redemption snapshots are retained for audit.
- Admin exports default to unredeemed accounts. CPA single-account export returns one auth JSON object; CPA multi-account export downloads a ZIP package with one auth JSON file per account. Sub2API export returns `{ exported_at, proxies, accounts }`.
- Redeem-code batch status exports from the admin console are CSV files. Full redeem codes are encrypted at rest and can be copied/exported again from the admin console for newly generated batches; legacy batches without stored ciphertext can only show the masked fallback.

## API

- `GET /health`
- `POST /api/alalalateam/accounts/import`
- `GET /api/alalalateam/accounts`
- `POST /api/alalalateam/accounts/probe`
- `POST /api/alalalateam/accounts/refresh`
- `POST /api/alalalateam/accounts/export`
- `GET /api/alalalateam/settings/auto-probe`
- `POST /api/alalalateam/settings/auto-probe`
- `POST /api/alalalateam/settings/auto-probe/run`
- `POST /api/alalalateam/redeem-code-batches`
- `GET /api/alalalateam/redeem-code-batches`
- `GET /api/alalalateam/redeem-code-batches/{batch_id}/codes`
- `POST /api/redeem/export`
- `POST /api/redeem/after-sale`

Admin endpoints accept `Authorization: Bearer <AETHER_POOL_ADMIN_TOKEN>` or `x-admin-token`.
By default admin endpoints are locked when `AETHER_POOL_ADMIN_TOKEN` is empty. The API refuses known example admin tokens and empty/example encryption secrets unless `AETHER_POOL_ALLOW_INSECURE_DEV_CONFIG=1` is explicitly set for isolated local development. Use `AETHER_POOL_ALLOW_OPEN_ADMIN=1` only for isolated local development.
Cross-origin browser access is restricted by `AETHER_POOL_CORS_ORIGINS`, defaulting to local Vite origins.

The public web entry is `/` and only shows the redeem/export page. The management console is available at `/alalalateam` and is not linked from the public page.

## Development

```bash
cd aether-pool
export AETHER_POOL_ADMIN_TOKEN=dev-admin-token
export AETHER_POOL_SECRET_KEY=dev-only-secret-key
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

## Server deployment with Docker and Baota Nginx

The default Compose deployment runs both services in Docker:

- `api`: Rust/Axum API on container port `8318`.
- `web`: Nginx container serving the built Vue app and proxying `/api` plus `/health` to `api`.

Baota Nginx only needs to reverse proxy the public domain to the Docker `web` port. By default that port is bound to `127.0.0.1:8080`, so it does not conflict with Baota's 80/443 listeners.

```bash
cd aether-pool
cp .env.example .env
vim .env
# Replace AETHER_POOL_ADMIN_TOKEN and AETHER_POOL_SECRET_KEY before starting.

# Build and start both frontend and backend.
docker compose up -d --build api web
docker compose ps
```

Baota reverse proxy target:

```text
http://127.0.0.1:8080
```

If you edit the Baota Nginx site config manually, the only required rule is:

```nginx
location / {
  proxy_pass http://127.0.0.1:8080;
  proxy_set_header Host $host;
  proxy_set_header X-Real-IP $remote_addr;
  proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
  proxy_set_header X-Forwarded-Proto $scheme;
}
```

For same-domain deployment, keep `VITE_API_BASE=` empty in `.env` so the frontend calls `/api/...` through the `web` container. If API and frontend use different domains, set `VITE_API_BASE` to the API origin and set `AETHER_POOL_CORS_ORIGINS` to the frontend origin.

Update deployment after pulling new code:

```bash
cd aether-pool
docker compose up -d --build api web
```

Optional static-Baota mode is still available if you do not want a web container:

```bash
docker compose --profile build run --rm web-build
docker compose up -d --build api
```

Then set Baota's site root to `/absolute/path/to/aether-pool/deploy/web` and proxy `/api/` to `http://127.0.0.1:8318`.

SQLite schema upgrades are applied automatically at API startup. The after-sale upgrade adds `redeem_code_batches.after_sale_limit` and `redeem_after_sales`; no manual migration command is required for existing deployments.

## Verification

```bash
cd aether-pool
cargo test
cargo fmt --all -- --check
RUSTC_WRAPPER= cargo clippy --all-targets --all-features -- -D warnings

cd apps/web
npm run build
```
