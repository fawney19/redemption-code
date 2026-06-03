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
- Admin exports default to unredeemed accounts. CPA single-account export returns one auth JSON object; CPA multi-account export downloads a ZIP package with one auth JSON file per account. Sub2API export returns `{ exported_at, proxies, accounts }`.

## API

- `GET /health`
- `POST /api/admin/accounts/import`
- `GET /api/admin/accounts`
- `POST /api/admin/accounts/probe`
- `POST /api/admin/accounts/refresh`
- `POST /api/admin/accounts/export`
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

## Verification

```bash
cd aether-pool
cargo test
cargo fmt --all -- --check
RUSTC_WRAPPER= cargo clippy --all-targets --all-features -- -D warnings

cd apps/web
npm run build
```
