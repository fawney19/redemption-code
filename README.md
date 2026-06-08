# AetherPool

AetherPool is a standalone Codex/OpenAI OAuth account pool service extracted from Aether-style pool operations.

## Layout

This repository uses a simple frontend/backend split:

- `backend`: Rust + Axum backend. It contains the API server, static web serving, SQLite repository, migrations, account parsing, redeem code logic, and Docker runtime.
- `frontend`: Vue 3 + Vite frontend. It contains the public redeem page, admin UI, API client, and static build.
- `docker-compose.yml`: production-style orchestration for the single `api` service that serves both API routes and the built frontend.
- `Makefile`: local development orchestration for running backend and frontend together.

## Behavior

- Imported Codex accounts start as `available`, or `at_expired` when the access token is already expired.
- Codex accounts belong to an `account_pools` pool. Pools are only an inventory isolation label for workspace/account type; they do not add provider routing, endpoint selection, model mapping, or scheduling.
- Existing databases are upgraded with a default pool (`default`). Existing accounts and redeem-code batches are backfilled into that default pool.
- Admin imports and redeem-code batches can target a specific pool. Re-importing an unredeemed account can move it to the selected pool; redeemed accounts keep their original pool so historical redemptions stay stable.
- Redeem-code batches are bound to one pool. Public redeem and after-sale exports infer the pool from the submitted codes and never allocate replacement accounts from another pool.
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
- `GET /api/alalalateam/pools`
- `GET /api/alalalateam/account-pools`
- `POST /api/alalalateam/pools`
- `POST /api/alalalateam/pools/{pool_id}`
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
Pool-aware admin endpoints accept `pool_id` as documented by shape: account list and batch list use query `pool_id`; import, probe, refresh, admin export, and batch creation use JSON `pool_id`. When `pool_id` is omitted, legacy behavior is preserved: account/batch lists are global, while new imports and new batches use the default pool. Registrars can call `GET /api/alalalateam/account-pools?active_only=true` with the admin token to fetch selectable upload pools, then submit `POST /api/alalalateam/accounts/import` with body `{"pool_id":"<pool id>","credentials":"<auth json/text>"}`.
The admin password is read from `AETHER_POOL_ADMIN_TOKEN`. The API refuses to start when it is empty or still set to a known example value.
Cross-origin browser access is controlled by `AETHER_POOL_CORS_ORIGINS`. Same-origin deployments should leave it empty because the Rust server serves both the frontend and `/api`. Separate-origin deployments should set it to the frontend origin, for example `https://pool.example.com`.
Public redeem rate limiting uses the socket peer IP by default. Set `AETHER_POOL_TRUST_PROXY_HEADERS=1` only when a trusted reverse proxy strips client-supplied forwarding headers and injects `x-forwarded-for`, `x-real-ip`, or `cf-connecting-ip`. Async redeem jobs are queued before they run: `AETHER_POOL_MAX_QUEUED_REDEEM_JOBS` defaults to `64`, `AETHER_POOL_MAX_QUEUED_REDEEM_JOBS_PER_CLIENT` defaults to `4`, `AETHER_POOL_MAX_ACTIVE_REDEEM_JOBS` defaults to `8`, and `AETHER_POOL_MAX_ACTIVE_REDEEM_JOBS_PER_CLIENT` defaults to `2` so one source cannot occupy every running slot. Network refresh/probe work is also capped globally by `AETHER_POOL_REDEEM_NETWORK_CONCURRENCY`, defaulting to `128`.
OAuth refresh defaults to ChatGPT (`AETHER_POOL_OAUTH_CLIENT_ID=app_EMoamEEZ73f0CkXaXp7hrann`, `AETHER_POOL_OAUTH_TOKEN_URL=https://auth.openai.com/oauth/token`), so those environment variables can be omitted unless you need to override them.

The public web entry is `/` and only shows the redeem/export page. The management console is available at `/act` and is not linked from the public page.

## Development

```bash
cd redemption-code
export AETHER_POOL_SECRET_KEY=dev-only-secret-key
make dev
```

The API defaults to `127.0.0.1:8318`, SQLite at `data/aether-pool.sqlite3`, and local admin password `admin123` when started through `make dev`. Override it with `AETHER_POOL_ADMIN_TOKEN=your-admin-password make dev`.

Frontend:

```bash
cd redemption-code/frontend
npm install
npm run dev -- --port 5178
```

The Vite dev server proxies `/api` and `/health` to `VITE_DEV_API_TARGET`, defaulting to `http://127.0.0.1:8318`.

For fully separated local origins, set `VITE_API_BASE=http://127.0.0.1:8318` for the frontend and make sure the backend allows the frontend origin through `AETHER_POOL_CORS_ORIGINS`.

## Server deployment with Docker and Baota Nginx

The default Compose deployment runs a single container:

- `api`: Rust/Axum app on container port `8318`, serving `/api`, `/health`, and the built Vue frontend.

Baota Nginx can reverse proxy the public domain to the Docker `api` port. By default that port is published on all host interfaces as `0.0.0.0:8318`; set `AETHER_POOL_API_BIND=127.0.0.1` in `.env` if you want access only through a local reverse proxy.

```bash
cd redemption-code
cp .env.example .env
vim .env
# Replace AETHER_POOL_ADMIN_TOKEN and AETHER_POOL_SECRET_KEY before starting.

# Build and start the app.
docker compose up -d --build api
docker compose ps
```

Baota reverse proxy target:

```text
http://127.0.0.1:8318
```

If you edit the Baota Nginx site config manually, the only required rule is:

```nginx
location / {
  proxy_pass http://127.0.0.1:8318;
  proxy_set_header Host $host;
  proxy_set_header X-Real-IP $remote_addr;
  proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
  proxy_set_header X-Forwarded-Proto $scheme;
}
```

With the default same-origin mode, keep `VITE_API_BASE=` empty. The frontend calls `/api/...` on the same Rust server.

For true separate-domain deployment, set:

```env
VITE_API_BASE=https://api.example.com
AETHER_POOL_CORS_ORIGINS=https://pool.example.com
```

Then expose the `api` service behind the API domain and serve a separate frontend static build behind the frontend domain.

Update deployment after pulling new code:

```bash
cd redemption-code
docker compose up -d --build api
```

SQLite schema upgrades are applied automatically at API startup. Current upgrades add `account_pools`, `accounts.pool_id`, `redeem_code_batches.pool_id`, `redeem_code_batches.after_sale_limit`, and `redeem_after_sales`; no manual migration command is required for existing deployments.

## Verification

```bash
cd redemption-code
cargo test
# Optional targeted async redeem job queue stress test.
make test-stress-jobs
# Optional mocked end-to-end redeem chain stress test, including probe network calls.
make test-stress-redeem-chain
cargo fmt --all -- --check
RUSTC_WRAPPER= cargo clippy --all-targets --all-features -- -D warnings

cd frontend
npm run build
```
