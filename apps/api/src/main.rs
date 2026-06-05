use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use account_pool_core::{
    access_token_needs_refresh, export_cpa_zip_from_document, normalize_wham_usage_response,
    parse_codex_accounts, unix_now_secs, AccountStatus, CodexAuthFile, ExportFormat,
    HealthCheckResult, ACCESS_TOKEN_REFRESH_GRACE_SECONDS, CHATGPT_ACCOUNTS_CHECK_URL,
    CHATGPT_SESSION_URL, CODEX_WHAM_USAGE_URL, CPA_PROBE_USER_AGENT, OPENAI_BROWSER_USER_AGENT,
    OPENAI_OAUTH_REFRESH_SCOPE,
};
use account_pool_data::{
    AccountListQuery, AccountPoolRepository, AccountPoolUpsertInput, AccountSummary,
    AutoProbeSettings, CreateRedeemBatchInput, DataError, RedeemRateLimitSettings,
};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::{Client, Proxy};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

const MAX_JSON_BODY_BYTES: usize = 10 * 1024 * 1024;
const MAX_REDEEM_CODES_PER_REQUEST: usize = 500;
const MAX_REDEEM_ACCOUNTS_PER_REQUEST: usize = 1_000;
const ADMIN_TOKEN_PLACEHOLDERS: &[&str] = &["change-this-admin-password"];
const SECRET_KEY_PLACEHOLDERS: &[&str] = &[
    "change-this-long-random-secret",
    "change-me-before-production",
    "aether-pool-local-development-secret",
];

#[derive(Clone)]
struct AppState {
    repo: AccountPoolRepository,
    http: Client,
    admin_token: Arc<String>,
    allow_open_admin: bool,
    oauth_client_id: Arc<String>,
    oauth_token_url: Arc<String>,
    chatgpt_check_url: Arc<String>,
    chatgpt_session_url: Arc<String>,
    wham_usage_url: Arc<String>,
    ip_check_url: Arc<String>,
    trust_proxy_headers: bool,
    auto_probe_lock: Arc<Mutex<()>>,
    redeem_rate_limiter: Arc<Mutex<RedeemRateLimiter>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aether_pool_api=info,tower_http=info".into()),
        )
        .init();

    let database_url = std::env::var("AETHER_POOL_DATABASE_URL")
        .unwrap_or_else(|_| "sqlite://data/aether-pool.sqlite3".to_string());
    let secret_key = std::env::var("AETHER_POOL_SECRET_KEY").unwrap_or_default();
    let admin_token = std::env::var("AETHER_POOL_ADMIN_TOKEN").unwrap_or_default();
    let allow_open_admin = env_flag("AETHER_POOL_ALLOW_OPEN_ADMIN");
    let allow_insecure_dev_config = env_flag("AETHER_POOL_ALLOW_INSECURE_DEV_CONFIG");
    validate_startup_secrets(
        &admin_token,
        &secret_key,
        allow_open_admin,
        allow_insecure_dev_config,
    )?;
    if admin_token.trim().is_empty() && !allow_open_admin {
        tracing::warn!(
            "AETHER_POOL_ADMIN_TOKEN is empty; admin endpoints are locked until a token is configured"
        );
    }
    let repo = AccountPoolRepository::connect(&database_url, &secret_key).await?;
    let state = AppState {
        repo,
        http: Client::builder()
            .user_agent("AetherPool/0.1")
            .timeout(std::time::Duration::from_secs(30))
            .build()?,
        admin_token: Arc::new(admin_token),
        allow_open_admin,
        oauth_client_id: Arc::new(
            std::env::var("AETHER_POOL_OAUTH_CLIENT_ID")
                .unwrap_or_else(|_| "app_EMoamEEZ73f0CkXaXp7hrann".to_string()),
        ),
        oauth_token_url: Arc::new(
            std::env::var("AETHER_POOL_OAUTH_TOKEN_URL")
                .unwrap_or_else(|_| "https://auth.openai.com/oauth/token".to_string()),
        ),
        chatgpt_check_url: Arc::new(
            std::env::var("AETHER_POOL_CHATGPT_CHECK_URL")
                .unwrap_or_else(|_| CHATGPT_ACCOUNTS_CHECK_URL.to_string()),
        ),
        chatgpt_session_url: Arc::new(
            std::env::var("AETHER_POOL_CHATGPT_SESSION_URL")
                .unwrap_or_else(|_| CHATGPT_SESSION_URL.to_string()),
        ),
        wham_usage_url: Arc::new(
            std::env::var("AETHER_POOL_WHAM_USAGE_URL")
                .unwrap_or_else(|_| CODEX_WHAM_USAGE_URL.to_string()),
        ),
        ip_check_url: Arc::new(
            std::env::var("AETHER_POOL_IP_CHECK_URL")
                .unwrap_or_else(|_| "https://api.ipify.org?format=json".to_string()),
        ),
        trust_proxy_headers: env_flag("AETHER_POOL_TRUST_PROXY_HEADERS"),
        auto_probe_lock: Arc::new(Mutex::new(())),
        redeem_rate_limiter: Arc::new(Mutex::new(RedeemRateLimiter::default())),
    };

    let _auto_probe_worker = spawn_auto_probe_worker(state.clone());

    let app = router(state)
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES))
        .layer(cors_layer());

    let addr: SocketAddr = std::env::var("AETHER_POOL_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8318".to_string())
        .parse()?;
    tracing::info!(%addr, "starting AetherPool API");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn validate_startup_secrets(
    admin_token: &str,
    secret_key: &str,
    allow_open_admin: bool,
    allow_insecure_dev_config: bool,
) -> anyhow::Result<()> {
    let admin_token = admin_token.trim();
    let secret_key = secret_key.trim();
    if secret_key.is_empty() || SECRET_KEY_PLACEHOLDERS.contains(&secret_key) {
        if !allow_insecure_dev_config {
            anyhow::bail!(
                "AETHER_POOL_SECRET_KEY must be set to a non-placeholder value; set AETHER_POOL_ALLOW_INSECURE_DEV_CONFIG=1 only for isolated local development"
            );
        }
        tracing::warn!("using insecure development secret key");
    }
    if !allow_open_admin
        && ADMIN_TOKEN_PLACEHOLDERS.contains(&admin_token)
        && !allow_insecure_dev_config
    {
        anyhow::bail!(
            "AETHER_POOL_ADMIN_TOKEN is still the example placeholder; set a private admin password before starting"
        );
    }
    Ok(())
}

fn cors_layer() -> CorsLayer {
    let origins = std::env::var("AETHER_POOL_CORS_ORIGINS").unwrap_or_else(|_| {
        [
            "http://127.0.0.1:5178",
            "http://localhost:5178",
            "http://127.0.0.1:5173",
            "http://localhost:5173",
        ]
        .join(",")
    });
    let origins = origins
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .filter_map(|origin| HeaderValue::from_str(origin).ok())
        .collect::<Vec<_>>();
    let allow_origin = if origins.is_empty() {
        AllowOrigin::exact(HeaderValue::from_static("http://127.0.0.1:5178"))
    } else {
        AllowOrigin::list(origins)
    };
    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static("x-admin-token"),
        ])
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route(
            "/api/alalalateam/pools",
            get(list_account_pools).post(create_account_pool),
        )
        .route("/api/alalalateam/account-pools", get(list_account_pools))
        .route(
            "/api/alalalateam/pools/{pool_id}",
            post(update_account_pool),
        )
        .route("/api/alalalateam/accounts/import", post(import_accounts))
        .route("/api/alalalateam/accounts", get(list_accounts))
        .route("/api/alalalateam/accounts/probe", post(probe_accounts))
        .route("/api/alalalateam/accounts/refresh", post(refresh_accounts))
        .route(
            "/api/alalalateam/accounts/export",
            post(export_admin_accounts),
        )
        .route("/api/alalalateam/accounts/delete", post(delete_accounts))
        .route(
            "/api/alalalateam/settings/auto-probe",
            get(get_auto_probe_settings).post(update_auto_probe_settings),
        )
        .route(
            "/api/alalalateam/settings/auto-probe/run",
            post(run_auto_probe_once),
        )
        .route(
            "/api/alalalateam/settings/redeem-rate-limit",
            get(get_redeem_rate_limit_settings).post(update_redeem_rate_limit_settings),
        )
        .route(
            "/api/alalalateam/settings/proxy/test",
            post(test_proxy_egress),
        )
        .route(
            "/api/alalalateam/settings/cpa/test",
            post(test_cpa_connection),
        )
        .route("/api/alalalateam/cpa/scan-401", post(scan_cpa_401))
        .route(
            "/api/alalalateam/redeem-code-batches",
            post(create_redeem_batch).get(list_redeem_batches),
        )
        .route(
            "/api/alalalateam/redeem-code-batches/{batch_id}/codes",
            get(list_redeem_codes),
        )
        .route("/api/redeem/export", post(redeem_export))
        .route("/api/redeem/after-sale", post(redeem_after_sale_export))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "aether-pool-api"
    }))
}

#[derive(Debug, Deserialize)]
struct PoolScopedQuery {
    pool_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListPoolsQuery {
    active_only: Option<bool>,
}

async fn list_account_pools(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListPoolsQuery>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let mut items = state.repo.list_account_pools().await?;
    if query.active_only.unwrap_or(false) {
        items.retain(|pool| pool.is_active);
    }
    let default_pool_id = state.repo.default_account_pool_id().await?;
    Ok(Json(json!({
        "success": true,
        "items": items,
        "default_pool_id": default_pool_id,
    })))
}

async fn create_account_pool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AccountPoolUpsertInput>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let pool = state.repo.create_account_pool(payload).await?;
    Ok(Json(json!({
        "success": true,
        "pool": pool,
    })))
}

async fn update_account_pool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(pool_id): Path<String>,
    Json(payload): Json<AccountPoolUpsertInput>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let pool = state.repo.update_account_pool(&pool_id, payload).await?;
    Ok(Json(json!({
        "success": true,
        "pool": pool,
    })))
}

#[derive(Debug, Deserialize)]
struct ImportAccountsRequest {
    credentials: String,
    pool_id: Option<String>,
}

async fn import_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ImportAccountsRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let parsed = parse_codex_accounts(&payload.credentials);
    let outcome = state
        .repo
        .import_accounts_into_pool(&parsed.accounts, payload.pool_id.as_deref())
        .await?;
    Ok(Json(json!({
        "success": true,
        "imported": outcome.imported,
        "updated": outcome.updated,
        "parse_errors": parsed.errors,
    })))
}

async fn list_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AccountListQuery>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let page = state.repo.list_accounts(query).await?;
    Ok(Json(json!(page)))
}

#[derive(Debug, Deserialize)]
struct AccountIdRequest {
    account_ids: Option<Vec<String>>,
    pool_id: Option<String>,
}

async fn refresh_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AccountIdRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let outcome = refresh_expired_accounts(
        &state,
        payload.account_ids.as_deref(),
        payload.pool_id.as_deref(),
        true,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "refreshed": outcome.refreshed,
        "skipped": outcome.skipped,
        "failed": outcome.failed,
        "results": outcome.results,
    })))
}

async fn delete_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AccountIdRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let account_ids = payload
        .account_ids
        .unwrap_or_default()
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if account_ids.is_empty() {
        return Err(ApiError::bad_request("account_ids is required"));
    }
    let outcome = state.repo.delete_unbound_accounts(&account_ids).await?;
    Ok(Json(json!({
        "success": true,
        "deleted": outcome.deleted,
        "skipped": outcome.skipped,
        "not_found": outcome.not_found,
        "results": outcome.results,
    })))
}

async fn probe_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AccountIdRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let settings = state.repo.get_auto_probe_settings().await?;
    let summary = run_probe_accounts(
        &state,
        payload.account_ids.as_deref(),
        ProbeRunOptions {
            max_accounts: None,
            concurrency: settings.concurrency as usize,
            include_redeemed: payload.account_ids.is_some(),
            pool_id: pool_scope_for_ids(payload.account_ids.as_deref(), payload.pool_id.as_deref()),
        },
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "checked": summary.checked,
        "failed": summary.failed,
        "results": summary.results,
    })))
}

async fn get_auto_probe_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let settings = state.repo.get_auto_probe_settings().await?;
    Ok(Json(auto_probe_settings_payload(settings)))
}

#[derive(Debug, Deserialize)]
struct AutoProbeSettingsPatch {
    enabled: Option<bool>,
    interval_seconds: Option<u64>,
    max_accounts_per_run: Option<u64>,
    concurrency: Option<u64>,
    refresh_before_probe: Option<bool>,
    probe_mode: Option<String>,
    deep_check_enabled: Option<bool>,
    cpa_base_url: Option<Option<String>>,
    cpa_management_key: Option<String>,
    proxy_enabled: Option<bool>,
    proxy_mode: Option<String>,
    proxy_url: Option<Option<String>>,
    proxy_api_url: Option<Option<String>>,
    proxy_default_scheme: Option<String>,
}

async fn update_auto_probe_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AutoProbeSettingsPatch>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let mut settings = state.repo.get_auto_probe_settings().await?;
    if let Some(management_key) = payload
        .cpa_management_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        state.repo.save_cpa_management_key(management_key).await?;
    }
    apply_auto_probe_settings_patch(&mut settings, payload);
    let settings = state.repo.save_auto_probe_settings(&settings).await?;
    Ok(Json(auto_probe_settings_payload(settings)))
}

#[derive(Debug, Deserialize)]
struct RedeemRateLimitSettingsPatch {
    enabled: Option<bool>,
    window_seconds: Option<u64>,
    max_requests: Option<u64>,
    whitelist_ips: Option<Vec<String>>,
}

async fn get_redeem_rate_limit_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let settings = state.repo.get_redeem_rate_limit_settings().await?;
    Ok(Json(redeem_rate_limit_settings_payload(settings)))
}

async fn update_redeem_rate_limit_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RedeemRateLimitSettingsPatch>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let mut settings = state.repo.get_redeem_rate_limit_settings().await?;
    if let Some(enabled) = payload.enabled {
        settings.enabled = enabled;
    }
    if let Some(window_seconds) = payload.window_seconds {
        settings.window_seconds = window_seconds;
    }
    if let Some(max_requests) = payload.max_requests {
        settings.max_requests = max_requests;
    }
    if let Some(whitelist_ips) = payload.whitelist_ips {
        settings.whitelist_ips = whitelist_ips;
    }
    let settings = state
        .repo
        .save_redeem_rate_limit_settings(&settings)
        .await?;
    state.redeem_rate_limiter.lock().await.clear();
    Ok(Json(redeem_rate_limit_settings_payload(settings)))
}

async fn test_proxy_egress(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AutoProbeSettingsPatch>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let mut settings = state.repo.get_auto_probe_settings().await?;
    apply_auto_probe_settings_patch(&mut settings, payload);
    let settings = settings.normalized();
    let started = Instant::now();
    let (http, proxy) = resolve_probe_http_client(&state, &settings).await?;
    let response = http
        .get(state.ip_check_url.as_str())
        .header("accept", "application/json,text/plain,*/*")
        .send()
        .await
        .map_err(|error| ApiError::bad_request(format!("IP 出口查询失败: {error}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| ApiError::bad_request(format!("IP 出口响应读取失败: {error}")))?;
    if !status.is_success() {
        return Err(ApiError::bad_request(format!(
            "IP 出口查询返回 {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    let ip = extract_ip_from_body(&body)
        .ok_or_else(|| ApiError::bad_request("IP 出口响应中没有可识别的 IP"))?;
    Ok(Json(json!({
        "success": true,
        "ip": ip,
        "proxy": proxy,
        "mode": if settings.proxy_enabled { settings.proxy_mode } else { "direct".to_string() },
        "url": state.ip_check_url.as_str(),
        "elapsed_ms": started.elapsed().as_millis() as u64,
    })))
}

async fn test_cpa_connection(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AutoProbeSettingsPatch>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let mut settings = state.repo.get_auto_probe_settings().await?;
    let payload_management_key = payload
        .cpa_management_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    apply_auto_probe_settings_patch(&mut settings, payload);
    let settings = settings.normalized();
    let management_key = match payload_management_key {
        Some(value) => value,
        None => state
            .repo
            .get_cpa_management_key()
            .await?
            .ok_or_else(|| ApiError::bad_request("CPA management key is required"))?,
    };
    let base_url = cpa_base_url(&settings)?;
    let started = Instant::now();
    let files = cpa_list_auth_files(&state.http, &base_url, &management_key).await?;
    Ok(Json(json!({
        "success": true,
        "base_url": base_url,
        "auth_file_count": files.len(),
        "elapsed_ms": started.elapsed().as_millis() as u64,
    })))
}

#[derive(Debug, Deserialize)]
struct CpaScanRequest {
    max_items: Option<usize>,
}

async fn scan_cpa_401(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CpaScanRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let settings = state.repo.get_auto_probe_settings().await?;
    let management_key = state
        .repo
        .get_cpa_management_key()
        .await?
        .ok_or_else(|| ApiError::bad_request("CPA management key is required"))?;
    let base_url = cpa_base_url(&settings)?;
    let max_items = payload.max_items.unwrap_or(20).clamp(1, 50);
    let files = cpa_list_auth_files(&state.http, &base_url, &management_key).await?;
    let candidates = files.into_iter().take(max_items).collect::<Vec<_>>();
    let mut results = Vec::new();
    for item in &candidates {
        results.push(cpa_probe_item(&state.http, &base_url, &management_key, item).await);
    }
    let diagnostics = results
        .iter()
        .filter(|item| {
            item.get("action").and_then(Value::as_str) != Some("ready")
                || item
                    .get("status_code")
                    .and_then(Value::as_u64)
                    .is_some_and(|status| matches!(status, 401 | 403 | 429))
        })
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "success": true,
        "total": candidates.len(),
        "max_items": max_items,
        "results": results,
        "diagnostics": diagnostics,
        "summary": {
            "total": candidates.len(),
            "ready": results.iter().filter(|item| item.get("action").and_then(Value::as_str) == Some("ready")).count(),
            "error_accounts": results.iter().filter(|item| item.get("ok").and_then(Value::as_bool) == Some(false)).count(),
            "failed": results.iter().filter(|item| item.get("action").and_then(Value::as_str) == Some("probe_failed")).count(),
        }
    })))
}

fn apply_auto_probe_settings_patch(
    settings: &mut AutoProbeSettings,
    payload: AutoProbeSettingsPatch,
) {
    if let Some(enabled) = payload.enabled {
        settings.enabled = enabled;
    }
    if let Some(interval_seconds) = payload.interval_seconds {
        settings.interval_seconds = interval_seconds;
    }
    if let Some(max_accounts_per_run) = payload.max_accounts_per_run {
        settings.max_accounts_per_run = max_accounts_per_run;
    }
    if let Some(concurrency) = payload.concurrency {
        settings.concurrency = concurrency;
    }
    if let Some(refresh_before_probe) = payload.refresh_before_probe {
        settings.refresh_before_probe = refresh_before_probe;
    }
    if let Some(probe_mode) = payload.probe_mode {
        settings.probe_mode = probe_mode;
    }
    if let Some(deep_check_enabled) = payload.deep_check_enabled {
        settings.deep_check_enabled = deep_check_enabled;
    }
    if let Some(cpa_base_url) = payload.cpa_base_url {
        settings.cpa_base_url = cpa_base_url;
    }
    if let Some(proxy_enabled) = payload.proxy_enabled {
        settings.proxy_enabled = proxy_enabled;
    }
    if let Some(proxy_mode) = payload.proxy_mode {
        settings.proxy_mode = proxy_mode;
    }
    if let Some(proxy_url) = payload.proxy_url {
        settings.proxy_url = proxy_url;
    }
    if let Some(proxy_api_url) = payload.proxy_api_url {
        settings.proxy_api_url = proxy_api_url;
    }
    if let Some(proxy_default_scheme) = payload.proxy_default_scheme {
        settings.proxy_default_scheme = proxy_default_scheme;
    }
}

async fn run_auto_probe_once(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let _guard = state.auto_probe_lock.lock().await;
    let settings = state.repo.get_auto_probe_settings().await?;
    let started_at = unix_now_secs();
    state.repo.mark_auto_probe_started(started_at).await?;
    let summary = match run_probe_accounts(
        &state,
        None,
        ProbeRunOptions {
            max_accounts: Some(settings.max_accounts_per_run as usize),
            concurrency: settings.concurrency as usize,
            include_redeemed: false,
            pool_id: None,
        },
    )
    .await
    {
        Ok(summary) => summary,
        Err(error) => {
            let message = error.message.clone();
            let _ = state
                .repo
                .mark_auto_probe_finished(
                    unix_now_secs(),
                    0,
                    json!({ "success": false, "error": message }),
                    Some(message),
                )
                .await;
            return Err(error);
        }
    };
    let result = probe_run_payload(&summary, 200);
    let settings = state
        .repo
        .mark_auto_probe_finished(
            unix_now_secs(),
            summary.checked as u64,
            result.clone(),
            None,
        )
        .await?;
    Ok(Json(json!({
        "success": true,
        "settings": auto_probe_settings_payload(settings),
        "run": result,
    })))
}

#[derive(Debug, Deserialize)]
struct ExportAccountsRequest {
    account_ids: Option<Vec<String>>,
    include_redeemed: Option<bool>,
    pool_id: Option<String>,
    format: String,
}

async fn export_admin_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ExportAccountsRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let format = payload
        .format
        .parse::<ExportFormat>()
        .map_err(|_| ApiError::bad_request("unsupported export format"))?;
    let include_redeemed = payload.include_redeemed.unwrap_or(false);
    if !include_redeemed {
        let _ = refresh_expired_accounts(
            &state,
            payload.account_ids.as_deref(),
            payload.pool_id.as_deref(),
            false,
        )
        .await?;
    }
    let document = state
        .repo
        .export_admin_accounts_scoped(
            payload.account_ids.as_deref(),
            include_redeemed,
            format,
            payload.pool_id.as_deref(),
        )
        .await?;
    let download = export_download(format, &document, "aether-pool-admin");
    Ok(Json(json!({
        "success": true,
        "format": format.as_str(),
        "document": document,
        "download": download,
    })))
}

#[derive(Debug, Deserialize)]
struct CreateRedeemBatchRequest {
    pool_id: Option<String>,
    name: String,
    total_count: usize,
    accounts_per_code: usize,
    after_sale_limit: Option<usize>,
    expires_at: Option<u64>,
    plan_filter: Option<Vec<String>>,
}

async fn create_redeem_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateRedeemBatchRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    if payload.name.trim().is_empty() {
        return Err(ApiError::bad_request("batch name is required"));
    }
    if payload.total_count == 0 || payload.total_count > 5000 {
        return Err(ApiError::bad_request("total_count must be 1..=5000"));
    }
    if payload.accounts_per_code == 0 || payload.accounts_per_code > 100 {
        return Err(ApiError::bad_request("accounts_per_code must be 1..=100"));
    }
    if payload.after_sale_limit.unwrap_or(1) > 10 {
        return Err(ApiError::bad_request("after_sale_limit must be 0..=10"));
    }
    let input = CreateRedeemBatchInput {
        name: payload.name,
        total_count: payload.total_count,
        accounts_per_code: payload.accounts_per_code,
        after_sale_limit: payload.after_sale_limit,
        expires_at: payload.expires_at,
        plan_filter: payload.plan_filter,
    };
    let outcome = state
        .repo
        .create_redeem_batch_in_pool(input, payload.pool_id.as_deref())
        .await?;
    Ok(Json(json!({
        "success": true,
        "batch_id": outcome.batch_id,
        "codes": outcome.codes,
    })))
}

async fn list_redeem_batches(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PoolScopedQuery>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let items = state
        .repo
        .list_redeem_batches_scoped(query.pool_id.as_deref())
        .await?;
    Ok(Json(json!({ "items": items })))
}

async fn list_redeem_codes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(batch_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let items = state.repo.list_redeem_codes(&batch_id).await?;
    Ok(Json(json!({ "items": items })))
}

#[derive(Debug, Deserialize)]
struct RedeemExportRequest {
    codes: Vec<String>,
    format: String,
}

#[derive(Debug, Deserialize)]
struct RedeemAfterSaleRequest {
    codes: Vec<String>,
    format: String,
}

async fn redeem_export(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<RedeemExportRequest>,
) -> Result<Json<Value>, ApiError> {
    enforce_redeem_rate_limit(&state, &headers, peer_addr.ip()).await?;
    validate_redeem_export_limits(payload.codes.len(), 0)?;
    let format = payload
        .format
        .parse::<ExportFormat>()
        .map_err(|_| ApiError::bad_request("unsupported export format"))?;
    let preparation = state.repo.prepare_redeem_export(&payload.codes).await?;
    validate_redeem_export_limits(payload.codes.len(), preparation.estimated_account_count)?;
    if !preparation.refresh_account_ids.is_empty() {
        let _ =
            refresh_expired_accounts(&state, Some(&preparation.refresh_account_ids), None, true)
                .await?;
    }
    if !preparation.probe_account_ids.is_empty() {
        let settings = state.repo.get_auto_probe_settings().await?;
        let probe_summary = run_probe_accounts(
            &state,
            Some(&preparation.probe_account_ids),
            ProbeRunOptions {
                max_accounts: None,
                concurrency: settings.concurrency as usize,
                include_redeemed: false,
                pool_id: None,
            },
        )
        .await?;
        if probe_summary.failed > 0 {
            return Err(ApiError::bad_request("兑换前测活失败，请稍后重试"));
        }
    }
    let outcome = state
        .repo
        .redeem_codes_for_export_with_verified_accounts(
            &payload.codes,
            format,
            Some(&preparation.probe_account_ids),
        )
        .await?;
    Ok(Json(json!({
        "success": true,
        "format": outcome.format.as_str(),
        "document": outcome.document,
        "download": export_download(outcome.format, &outcome.document, "aether-pool-redeem"),
        "successes": outcome.successes,
        "failures": outcome.failures,
    })))
}

async fn redeem_after_sale_export(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<RedeemAfterSaleRequest>,
) -> Result<Json<Value>, ApiError> {
    enforce_redeem_rate_limit(&state, &headers, peer_addr.ip()).await?;
    validate_redeem_export_limits(payload.codes.len(), 0)?;
    let format = payload
        .format
        .parse::<ExportFormat>()
        .map_err(|_| ApiError::bad_request("unsupported export format"))?;
    let preparation = state.repo.prepare_after_sale_export(&payload.codes).await?;
    validate_redeem_export_limits(payload.codes.len(), preparation.estimated_account_count)?;
    if !preparation.refresh_account_ids.is_empty() {
        let _ =
            refresh_expired_accounts(&state, Some(&preparation.refresh_account_ids), None, true)
                .await?;
    }
    if !preparation.probe_account_ids.is_empty() {
        let settings = state.repo.get_auto_probe_settings().await?;
        let probe_summary = run_probe_accounts(
            &state,
            Some(&preparation.probe_account_ids),
            ProbeRunOptions {
                max_accounts: None,
                concurrency: settings.concurrency as usize,
                include_redeemed: true,
                pool_id: None,
            },
        )
        .await?;
        if probe_summary.failed > 0 {
            return Err(ApiError::bad_request("售后测活失败，请稍后重试"));
        }
    }
    let outcome = state
        .repo
        .redeem_after_sale_for_export_with_verified_accounts(
            &payload.codes,
            format,
            Some(&preparation.probe_account_ids),
        )
        .await?;
    Ok(Json(json!({
        "success": true,
        "format": outcome.format.as_str(),
        "document": outcome.document,
        "download": export_download(outcome.format, &outcome.document, "aether-pool-after-sale"),
        "successes": outcome.successes,
        "failures": outcome.failures,
    })))
}

fn validate_redeem_export_limits(
    code_count: usize,
    estimated_account_count: usize,
) -> Result<(), ApiError> {
    if code_count == 0 {
        return Err(ApiError::bad_request("codes is required"));
    }
    if code_count > MAX_REDEEM_CODES_PER_REQUEST {
        return Err(ApiError::bad_request(format!(
            "codes must contain at most {MAX_REDEEM_CODES_PER_REQUEST} items"
        )));
    }
    if estimated_account_count > MAX_REDEEM_ACCOUNTS_PER_REQUEST {
        return Err(ApiError::bad_request(format!(
            "single redeem export can contain at most {MAX_REDEEM_ACCOUNTS_PER_REQUEST} accounts"
        )));
    }
    Ok(())
}

fn export_download(format: ExportFormat, document: &Value, prefix: &str) -> Option<Value> {
    match format {
        ExportFormat::Cpa => export_cpa_zip_from_document(document).map(|bytes| {
            json!({
                "filename": format!("{prefix}-cpa-accounts.zip"),
                "content_type": "application/zip",
                "encoding": "base64",
                "data": STANDARD.encode(bytes),
            })
        }),
        ExportFormat::Sub2api => None,
    }
}

fn spawn_auto_probe_worker(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = auto_probe_worker_tick(&state).await {
                tracing::warn!(error = ?error, "auto probe worker tick failed");
            }
        }
    })
}

async fn auto_probe_worker_tick(state: &AppState) -> Result<(), ApiError> {
    let settings = state.repo.get_auto_probe_settings().await?;
    if !settings.enabled {
        return Ok(());
    }
    let now = unix_now_secs();
    let last_run_at = settings
        .last_finished_at
        .or(settings.last_started_at)
        .unwrap_or_default();
    if last_run_at > 0 && now < last_run_at.saturating_add(settings.interval_seconds) {
        return Ok(());
    }
    let _guard = match state.auto_probe_lock.try_lock() {
        Ok(guard) => guard,
        Err(_) => return Ok(()),
    };
    state.repo.mark_auto_probe_started(now).await?;
    match run_probe_accounts(
        state,
        None,
        ProbeRunOptions {
            max_accounts: Some(settings.max_accounts_per_run as usize),
            concurrency: settings.concurrency as usize,
            include_redeemed: false,
            pool_id: None,
        },
    )
    .await
    {
        Ok(summary) => {
            let result = probe_run_payload(&summary, 50);
            state
                .repo
                .mark_auto_probe_finished(unix_now_secs(), summary.checked as u64, result, None)
                .await?;
        }
        Err(error) => {
            let message = error.message.clone();
            let _ = state
                .repo
                .mark_auto_probe_finished(
                    unix_now_secs(),
                    0,
                    json!({ "success": false, "error": message }),
                    Some(message),
                )
                .await;
            return Err(error);
        }
    }
    Ok(())
}

#[derive(Clone)]
struct ProbeRunOptions {
    max_accounts: Option<usize>,
    concurrency: usize,
    include_redeemed: bool,
    pool_id: Option<String>,
}

#[derive(Default)]
struct ProbeRunSummary {
    checked: usize,
    failed: usize,
    results: Vec<Value>,
}

async fn run_probe_accounts(
    state: &AppState,
    account_ids: Option<&[String]>,
    options: ProbeRunOptions,
) -> Result<ProbeRunSummary, ApiError> {
    let mut accounts = if options.include_redeemed {
        if let Some(account_ids) = account_ids {
            state
                .repo
                .load_auth_files_for_ids(account_ids, true)
                .await?
        } else {
            state
                .repo
                .load_unredeemed_auth_files_scoped(None, options.pool_id.as_deref())
                .await?
        }
    } else {
        state
            .repo
            .load_unredeemed_auth_files_scoped(account_ids, options.pool_id.as_deref())
            .await?
    };
    if let Some(max_accounts) = options.max_accounts {
        accounts.truncate(max_accounts);
    }
    let probe_settings = state.repo.get_auto_probe_settings().await?;
    let (probe_http, probe_proxy) = resolve_probe_http_client(state, &probe_settings).await?;
    let cpa_management_key = state.repo.get_cpa_management_key().await?;
    let concurrency = options.concurrency.clamp(1, 32);
    let mut join_set = JoinSet::new();
    let mut summary = ProbeRunSummary::default();
    for (account, auth_file) in accounts {
        while join_set.len() >= concurrency {
            collect_probe_result(&mut join_set, &mut summary).await;
        }
        let state = state.clone();
        let probe_http = probe_http.clone();
        let probe_proxy = probe_proxy.clone();
        let probe_settings = probe_settings.clone();
        let cpa_management_key = cpa_management_key.clone();
        join_set.spawn(async move {
            probe_one_account(
                state,
                account,
                auth_file,
                probe_http,
                probe_proxy,
                probe_settings,
                cpa_management_key,
            )
            .await
        });
    }
    while !join_set.is_empty() {
        collect_probe_result(&mut join_set, &mut summary).await;
    }
    Ok(summary)
}

fn pool_scope_for_ids(account_ids: Option<&[String]>, pool_id: Option<&str>) -> Option<String> {
    if account_ids.is_some() {
        return None;
    }
    pool_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

async fn collect_probe_result(
    join_set: &mut JoinSet<Result<Value, ApiError>>,
    summary: &mut ProbeRunSummary,
) {
    let Some(result) = join_set.join_next().await else {
        return;
    };
    summary.checked += 1;
    match result {
        Ok(Ok(value)) => summary.results.push(value),
        Ok(Err(error)) => {
            summary.failed += 1;
            summary.results.push(json!({
                "status": "probe_failed",
                "error": error.message,
            }));
        }
        Err(error) => {
            summary.failed += 1;
            summary.results.push(json!({
                "status": "probe_failed",
                "error": error.to_string(),
            }));
        }
    }
}

async fn probe_one_account(
    state: AppState,
    summary: AccountSummary,
    auth_file: CodexAuthFile,
    probe_http: Client,
    probe_proxy: Option<String>,
    settings: AutoProbeSettings,
    cpa_management_key: Option<String>,
) -> Result<Value, ApiError> {
    let started = Instant::now();
    let mut action = "probe_failed".to_string();
    let mut probe_source = "none".to_string();
    let mut http_status = None;
    let mut wham_snapshot = None;
    let mut diagnosis = None;
    let mut auth_updated = false;
    let mut result = if let Some(access_token) = auth_file
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let wham = run_wham_probe(
            &state,
            &probe_http,
            &settings,
            cpa_management_key.as_deref(),
            &auth_file,
            access_token,
        )
        .await;
        action = wham_action(&wham);
        probe_source = wham.source.clone();
        http_status = wham.http_status;
        wham_snapshot = Some(wham.snapshot());
        let needs_diagnosis =
            settings.deep_check_enabled && !matches!(wham.result.status, AccountStatus::Available);
        let mut result = wham.result.clone();
        if needs_diagnosis {
            let lifecycle = diagnose_lifecycle(&state, &probe_http, &auth_file).await;
            if let Some(ref refreshed) = lifecycle.auth_file {
                persist_refreshed_auth(&state, &summary, refreshed, &lifecycle).await?;
                auth_updated = true;
            }
            action = if lifecycle.ok {
                "diagnosed".to_string()
            } else {
                lifecycle.status.clone()
            };
            result = HealthCheckResult {
                status: lifecycle_account_status(&lifecycle.status),
                plan_type: lifecycle.plan_type.clone().or(result.plan_type),
                quota_snapshot: Some(json!({
                    "wham": wham_snapshot.as_ref(),
                    "diagnosis": lifecycle.public_payload(),
                })),
                error: if lifecycle.ok {
                    None
                } else {
                    Some(lifecycle.message.clone())
                },
            };
            diagnosis = Some(lifecycle);
        }
        result
    } else if settings.deep_check_enabled {
        let lifecycle = diagnose_lifecycle(&state, &probe_http, &auth_file).await;
        if let Some(ref refreshed) = lifecycle.auth_file {
            persist_refreshed_auth(&state, &summary, refreshed, &lifecycle).await?;
            auth_updated = true;
        }
        action = lifecycle.status.clone();
        probe_source = "direct_lifecycle".to_string();
        http_status = lifecycle.http_status;
        let result = HealthCheckResult {
            status: lifecycle_account_status(&lifecycle.status),
            plan_type: lifecycle.plan_type.clone().or(auth_file.plan_type.clone()),
            quota_snapshot: Some(json!({ "diagnosis": lifecycle.public_payload() })),
            error: if lifecycle.ok {
                None
            } else {
                Some(lifecycle.message.clone())
            },
        };
        diagnosis = Some(lifecycle);
        result
    } else {
        HealthCheckResult {
            status: AccountStatus::AuthInvalid,
            plan_type: auth_file.plan_type.clone(),
            quota_snapshot: None,
            error: Some("missing access_token".to_string()),
        }
    };
    if result.plan_type.is_none() {
        result.plan_type = auth_file.plan_type.clone();
    }
    let diagnosis_status = diagnosis.as_ref().map(|value| value.status.clone());
    let status_label = diagnosis
        .as_ref()
        .map(|value| value.status_label.clone())
        .unwrap_or_else(|| lifecycle_status_label(result.status.as_str()));
    let message = diagnosis
        .as_ref()
        .map(|value| value.message.clone())
        .or_else(|| result.error.clone())
        .unwrap_or_else(|| {
            if result.status == AccountStatus::Available {
                "账号可用".to_string()
            } else {
                status_label.clone()
            }
        });
    let credential_ok = diagnosis.as_ref().is_some_and(|value| value.credential_ok)
        || matches!(
            result.status,
            AccountStatus::Available | AccountStatus::QuotaExhausted
        );
    let usable = result.status == AccountStatus::Available;
    state
        .repo
        .record_health_check(
            &summary.id,
            &result,
            http_status,
            Some(started.elapsed().as_millis() as u64),
        )
        .await?;
    Ok(json!({
        "account_id": summary.id,
        "status": result.status.as_str(),
        "plan_type": result.plan_type,
        "http_status": http_status,
        "refresh": "not_attempted",
        "probe_source": probe_source,
        "action": action,
        "diagnosis_status": diagnosis_status,
        "status_label": status_label,
        "message": message,
        "credential_ok": credential_ok,
        "usable": usable,
        "auth_updated": auth_updated,
        "proxy": probe_proxy,
        "error": result.error,
        "wham": wham_snapshot,
        "diagnosis": diagnosis.map(|value| value.public_payload()),
    }))
}

async fn persist_refreshed_auth(
    state: &AppState,
    summary: &AccountSummary,
    refreshed: &CodexAuthFile,
    lifecycle: &LifecycleDiagnosis,
) -> Result<(), ApiError> {
    let refreshed_at = Some(unix_now_secs());
    if summary.redeemed_at.is_some() {
        state
            .repo
            .update_redeemed_account_auth_snapshot(&summary.id, refreshed, refreshed_at)
            .await?;
        return Ok(());
    }
    state
        .repo
        .update_account_auth(
            &summary.id,
            refreshed,
            lifecycle_account_status(&lifecycle.status),
            refreshed_at,
        )
        .await?;
    Ok(())
}

#[derive(Debug, Clone)]
struct WhamProbeOutcome {
    source: String,
    http_status: Option<u16>,
    body: Option<Value>,
    error: Option<String>,
    result: HealthCheckResult,
}

impl WhamProbeOutcome {
    fn snapshot(&self) -> Value {
        json!({
            "source": self.source,
            "http_status": self.http_status,
            "body": self.body,
            "error": self.error,
        })
    }
}

async fn run_wham_probe(
    state: &AppState,
    http: &Client,
    settings: &AutoProbeSettings,
    cpa_management_key: Option<&str>,
    auth_file: &CodexAuthFile,
    access_token: &str,
) -> WhamProbeOutcome {
    let auth_index = auth_index(auth_file);
    let can_use_cpa = settings.probe_mode != "direct"
        && auth_index.is_some()
        && settings.cpa_base_url.as_deref().is_some()
        && cpa_management_key.is_some();
    if can_use_cpa {
        let base_url = cpa_base_url(settings).unwrap_or_default();
        return cpa_wham_probe(
            &state.http,
            &base_url,
            cpa_management_key.unwrap_or_default(),
            auth_file,
            auth_index.as_deref().unwrap_or_default(),
        )
        .await;
    }
    if settings.probe_mode == "cpa" {
        let error = if auth_index.is_none() {
            "missing auth_index"
        } else if settings.cpa_base_url.as_deref().is_none() {
            "missing cpa_base_url"
        } else {
            "missing cpa_management_key"
        };
        return WhamProbeOutcome {
            source: "cpa".to_string(),
            http_status: None,
            body: None,
            error: Some(error.to_string()),
            result: HealthCheckResult {
                status: AccountStatus::RefreshFailed,
                plan_type: auth_file.plan_type.clone(),
                quota_snapshot: None,
                error: Some(error.to_string()),
            },
        };
    }
    direct_wham_probe(state, http, auth_file, access_token).await
}

async fn direct_wham_probe(
    state: &AppState,
    http: &Client,
    auth_file: &CodexAuthFile,
    access_token: &str,
) -> WhamProbeOutcome {
    let mut request = http
        .get(state.wham_usage_url.as_str())
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", CPA_PROBE_USER_AGENT)
        .bearer_auth(access_token);
    if let Some(account_id) = chatgpt_account_id(auth_file) {
        request = request.header("Chatgpt-Account-Id", account_id);
    }
    match request.send().await {
        Ok(response) => {
            let status_code = response.status().as_u16();
            let body = response_json_or_text(response).await;
            WhamProbeOutcome {
                source: "direct_wham".to_string(),
                http_status: Some(status_code),
                result: normalize_wham_usage_response(status_code, body.clone()),
                body,
                error: None,
            }
        }
        Err(error) => {
            let error = error.to_string();
            WhamProbeOutcome {
                source: "direct_wham".to_string(),
                http_status: None,
                body: None,
                error: Some(error.clone()),
                result: HealthCheckResult {
                    status: AccountStatus::RefreshFailed,
                    plan_type: auth_file.plan_type.clone(),
                    quota_snapshot: None,
                    error: Some(error),
                },
            }
        }
    }
}

async fn cpa_wham_probe(
    http: &Client,
    base_url: &str,
    management_key: &str,
    auth_file: &CodexAuthFile,
    auth_index: &str,
) -> WhamProbeOutcome {
    let payload = cpa_probe_payload(auth_file, auth_index);
    match http
        .post(format!("{base_url}/v0/management/api-call"))
        .headers(cpa_header_map(management_key))
        .json(&payload)
        .send()
        .await
    {
        Ok(response) => {
            let management_status = response.status().as_u16();
            let payload = response_json_or_text(response)
                .await
                .unwrap_or_else(|| json!({}));
            let status_code = cpa_payload_status_code(&payload).unwrap_or(management_status);
            let body = cpa_payload_body(&payload);
            WhamProbeOutcome {
                source: "cpa_api_call".to_string(),
                http_status: Some(status_code),
                result: normalize_wham_usage_response(status_code, body.clone()),
                body: Some(payload),
                error: None,
            }
        }
        Err(error) => {
            let error = error.to_string();
            WhamProbeOutcome {
                source: "cpa_api_call".to_string(),
                http_status: None,
                body: None,
                error: Some(error.clone()),
                result: HealthCheckResult {
                    status: AccountStatus::RefreshFailed,
                    plan_type: auth_file.plan_type.clone(),
                    quota_snapshot: None,
                    error: Some(error),
                },
            }
        }
    }
}

fn wham_action(wham: &WhamProbeOutcome) -> String {
    match wham.http_status {
        Some(200) => "ready".to_string(),
        Some(401) => "401".to_string(),
        Some(403) => "risk_blocked".to_string(),
        Some(429) => "usage_limit_reached".to_string(),
        Some(_) => "http_error".to_string(),
        None => "probe_failed".to_string(),
    }
}

#[derive(Debug, Clone)]
struct LifecycleDiagnosis {
    status: String,
    status_label: String,
    message: String,
    ok: bool,
    credential_ok: bool,
    usable: bool,
    plan_type: Option<String>,
    http_status: Option<u16>,
    access_token_updated: bool,
    refresh_token_rotated: bool,
    auth_file: Option<CodexAuthFile>,
    probe: Option<Value>,
}

impl LifecycleDiagnosis {
    fn public_payload(&self) -> Value {
        json!({
            "status": self.status,
            "status_label": self.status_label,
            "message": self.message,
            "ok": self.ok,
            "credential_ok": self.credential_ok,
            "usable": self.usable,
            "plan_type": self.plan_type,
            "http_status": self.http_status,
            "access_token_updated": self.access_token_updated,
            "refresh_token_rotated": self.refresh_token_rotated,
            "probe": self.probe,
        })
    }
}

async fn diagnose_lifecycle(
    state: &AppState,
    http: &Client,
    auth_file: &CodexAuthFile,
) -> LifecycleDiagnosis {
    if let Some(refresh_token) = auth_file
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let refresh = refresh_openai_with_rt(state, http, auth_file, refresh_token).await;
        if let Ok(refreshed) = refresh {
            let probe = probe_openai_access_token(state, http, &refreshed).await;
            let mut diagnosis = if refreshed
                .refresh_token
                .as_deref()
                .is_some_and(|value| value != refresh_token)
            {
                lifecycle_diagnosis(
                    "rt_rotated",
                    "refresh_token 已刷新出新的 access_token",
                    true,
                )
            } else {
                lifecycle_diagnosis("refreshed", "refresh_token 已刷新出新的 access_token", true)
            };
            diagnosis.access_token_updated = true;
            diagnosis.refresh_token_rotated = diagnosis.status == "rt_rotated";
            diagnosis.auth_file = Some(refreshed);
            merge_probe_into_diagnosis(&mut diagnosis, probe);
            return diagnosis;
        }
        return classify_oauth_refresh_error(
            refresh
                .err()
                .unwrap_or_else(|| "refresh failed".to_string()),
        );
    }

    if let Some(session_token) = auth_file
        .session_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let session =
            refresh_openai_with_session_token(state, http, auth_file, session_token).await;
        match session {
            Ok(refreshed) => {
                let probe = probe_openai_access_token(state, http, &refreshed).await;
                let mut diagnosis = lifecycle_diagnosis(
                    "refreshed",
                    "session_token 已刷新出新的 access_token",
                    true,
                );
                diagnosis.access_token_updated = true;
                diagnosis.auth_file = Some(refreshed);
                merge_probe_into_diagnosis(&mut diagnosis, probe);
                return diagnosis;
            }
            Err(SessionTokenError { status, message }) => {
                if status == Some(401) {
                    return lifecycle_diagnosis("session_expired", "session_token 已失效", false);
                }
                if status == Some(403) {
                    return lifecycle_diagnosis(
                        "risk_blocked",
                        "session_token 探测触发风控或被拒绝",
                        false,
                    );
                }
                let mut diagnosis = lifecycle_diagnosis("probe_failed", message.as_str(), false);
                diagnosis.http_status = status;
                return diagnosis;
            }
        }
    }

    if auth_file
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some()
    {
        return probe_openai_access_token(state, http, auth_file).await;
    }

    lifecycle_diagnosis(
        "needs_login",
        "缺少 ChatGPT/Codex refresh_token、session_token 或 access_token",
        false,
    )
}

fn lifecycle_diagnosis(status: &str, message: &str, ok: bool) -> LifecycleDiagnosis {
    LifecycleDiagnosis {
        status: status.to_string(),
        status_label: lifecycle_status_label(status),
        message: message.to_string(),
        ok,
        credential_ok: ok,
        usable: status == "active" || status == "refreshed" || status == "rt_rotated",
        plan_type: None,
        http_status: None,
        access_token_updated: false,
        refresh_token_rotated: false,
        auth_file: None,
        probe: None,
    }
}

fn merge_probe_into_diagnosis(diagnosis: &mut LifecycleDiagnosis, probe: LifecycleDiagnosis) {
    diagnosis.probe = Some(probe.public_payload());
    if probe.status == "banned" {
        diagnosis.status = probe.status;
        diagnosis.status_label = probe.status_label;
        diagnosis.message = probe.message;
        diagnosis.ok = false;
        diagnosis.credential_ok = false;
        diagnosis.usable = false;
    } else if probe.status == "usage_limit_reached" {
        diagnosis.status = probe.status;
        diagnosis.status_label = probe.status_label;
        diagnosis.message = probe.message;
        diagnosis.ok = true;
        diagnosis.credential_ok = true;
        diagnosis.usable = false;
    }
    if diagnosis.plan_type.is_none() {
        diagnosis.plan_type = probe.plan_type;
    }
    if diagnosis.http_status.is_none() {
        diagnosis.http_status = probe.http_status;
    }
}

async fn refresh_openai_with_rt(
    state: &AppState,
    http: &Client,
    auth_file: &CodexAuthFile,
    refresh_token: &str,
) -> Result<CodexAuthFile, String> {
    let response = http
        .post(state.oauth_token_url.as_str())
        .header("Accept", "application/json")
        .header("User-Agent", OPENAI_BROWSER_USER_AGENT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", state.oauth_client_id.as_str()),
            ("refresh_token", refresh_token),
            ("scope", OPENAI_OAUTH_REFRESH_SCOPE),
        ])
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let body = response_json_or_text(response)
        .await
        .unwrap_or_else(|| json!({}));
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", compact_error_message(&body)));
    }
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "token response missing access_token".to_string())?;
    let mut next = auth_file.clone();
    next.access_token = Some(access_token.to_string());
    if let Some(refresh_token) = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        next.refresh_token = Some(refresh_token.to_string());
    }
    if let Some(id_token) = body
        .get("id_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        next.id_token = Some(id_token.to_string());
    }
    if let Some(expires_in) = body.get("expires_in").and_then(Value::as_u64) {
        next.expires_at = Some(json!(unix_now_secs().saturating_add(expires_in)));
    }
    next.last_refresh = Some(chrono::Utc::now().to_rfc3339());
    Ok(next.normalized())
}

#[derive(Debug)]
struct SessionTokenError {
    status: Option<u16>,
    message: String,
}

async fn refresh_openai_with_session_token(
    state: &AppState,
    http: &Client,
    auth_file: &CodexAuthFile,
    session_token: &str,
) -> Result<CodexAuthFile, SessionTokenError> {
    let cookie = format!(
        "__Secure-next-auth.session-token={session_token}; __Secure-authjs.session-token={session_token}"
    );
    let response = http
        .get(state.chatgpt_session_url.as_str())
        .header("Accept", "application/json")
        .header("User-Agent", OPENAI_BROWSER_USER_AGENT)
        .header("Referer", "https://chatgpt.com/")
        .header("Cookie", cookie)
        .send()
        .await
        .map_err(|error| SessionTokenError {
            status: None,
            message: error.to_string(),
        })?;
    let status = response.status().as_u16();
    let body = response_json_or_text(response)
        .await
        .unwrap_or_else(|| json!({}));
    if !(200..300).contains(&status) {
        return Err(SessionTokenError {
            status: Some(status),
            message: compact_error_message(&body),
        });
    }
    let access_token =
        first_string(&body, &["accessToken", "access_token"]).ok_or_else(|| SessionTokenError {
            status: Some(status),
            message: "session response missing access token".to_string(),
        })?;
    let mut next = auth_file.clone();
    next.access_token = Some(access_token);
    if let Some(refresh_token) = first_string(&body, &["refreshToken", "refresh_token"]) {
        next.refresh_token = Some(refresh_token);
    }
    if let Some(id_token) = first_string(&body, &["idToken", "id_token"]) {
        next.id_token = Some(id_token);
    }
    next.session_token = Some(session_token.to_string());
    next.last_refresh = Some(chrono::Utc::now().to_rfc3339());
    Ok(next.normalized())
}

async fn probe_openai_access_token(
    state: &AppState,
    http: &Client,
    auth_file: &CodexAuthFile,
) -> LifecycleDiagnosis {
    let Some(access_token) = auth_file
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return lifecycle_diagnosis("needs_login", "缺少 access_token", false);
    };
    let response = http
        .get(state.chatgpt_check_url.as_str())
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/json")
        .header("User-Agent", OPENAI_BROWSER_USER_AGENT)
        .header("Referer", "https://chatgpt.com/")
        .send()
        .await;
    let (status, body, raw_error) = match response {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = response_json_or_text(response).await;
            (Some(status), body, None)
        }
        Err(error) => (None, None, Some(error.to_string())),
    };
    let status_code = status.unwrap_or_default();
    let mut diagnosis = match status {
        Some(200) => lifecycle_diagnosis("active", "账号可用", true),
        Some(401) => lifecycle_diagnosis("session_expired", "access_token 已过期或被撤销", false),
        Some(403) => {
            let text = body
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if contains_any(
                &text,
                &[
                    "banned",
                    "deactivated",
                    "disabled",
                    "suspended",
                    "封禁",
                    "停用",
                ],
            ) {
                lifecycle_diagnosis("banned", "账号封禁/停用或触发风控", false)
            } else {
                lifecycle_diagnosis("risk_blocked", "账号封禁/停用或触发风控", false)
            }
        }
        Some(429) => {
            let text = body
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if text.contains("usage_limit_reached") || text.contains("usage limit has been reached")
            {
                let mut diagnosis =
                    lifecycle_diagnosis("usage_limit_reached", "额度耗尽但凭证有效", true);
                diagnosis.usable = false;
                diagnosis.credential_ok = true;
                diagnosis
            } else {
                lifecycle_diagnosis(
                    "probe_failed",
                    &format!("OpenAI 探测暂不可用：HTTP {status_code}"),
                    false,
                )
            }
        }
        Some(500 | 502 | 503 | 504) => lifecycle_diagnosis(
            "probe_failed",
            &format!("OpenAI 探测暂不可用：HTTP {status_code}"),
            false,
        ),
        Some(_) => lifecycle_diagnosis(
            "probe_failed",
            &format!("OpenAI 探测失败：HTTP {status_code}"),
            false,
        ),
        None => lifecycle_diagnosis(
            "probe_failed",
            raw_error.as_deref().unwrap_or("OpenAI 探测失败"),
            false,
        ),
    };
    diagnosis.http_status = status;
    diagnosis.plan_type = extract_check_plan_type(body.as_ref()).or(auth_file.plan_type.clone());
    diagnosis.probe = body.map(|value| json!({ "body": value }));
    diagnosis
}

fn classify_oauth_refresh_error(error: String) -> LifecycleDiagnosis {
    let lowered = error.to_ascii_lowercase();
    if contains_any(
        &lowered,
        &[
            "deactivated",
            "disabled",
            "banned",
            "suspended",
            "封禁",
            "停用",
        ],
    ) {
        lifecycle_diagnosis("banned", &error, false)
    } else if lowered.contains("http 403") {
        lifecycle_diagnosis("risk_blocked", &error, false)
    } else if contains_any(
        &lowered,
        &[
            "invalid_grant",
            "invalid_client",
            "unauthorized_client",
            "invalid_request",
            "token_expired",
            "http 400",
            "http 401",
        ],
    ) {
        lifecycle_diagnosis("rt_invalid", &error, false)
    } else {
        lifecycle_diagnosis("probe_failed", &error, false)
    }
}

fn lifecycle_account_status(status: &str) -> AccountStatus {
    match status {
        "active" | "refreshed" | "rt_rotated" => AccountStatus::Available,
        "usage_limit_reached" => AccountStatus::QuotaExhausted,
        "risk_blocked" => AccountStatus::Forbidden,
        "banned" | "session_expired" | "rt_invalid" | "needs_login" | "not_openai_auth" => {
            AccountStatus::AuthInvalid
        }
        "probe_failed" => AccountStatus::RefreshFailed,
        _ => AccountStatus::RefreshFailed,
    }
}

fn lifecycle_status_label(status: &str) -> String {
    match status {
        "active" | "available" => "可用",
        "refreshed" => "已刷新",
        "rt_rotated" => "已刷新并轮换 RT",
        "rt_invalid" => "RT 失效",
        "session_expired" => "会话失效",
        "banned" => "封禁/停用",
        "risk_blocked" | "forbidden" => "风控/受限",
        "usage_limit_reached" | "quota_exhausted" => "额度耗尽",
        "needs_login" => "需要重新授权",
        "probe_failed" | "refresh_failed" => "探测失败",
        "not_openai_auth" => "非 OpenAI 凭证",
        "auth_invalid" => "账号失效",
        "at_expired" => "AT 过期",
        other => other,
    }
    .to_string()
}

fn extract_check_plan_type(value: Option<&Value>) -> Option<String> {
    let account = value?
        .get("accounts")
        .and_then(|accounts| accounts.get("default"))?;
    let entitlement = account.get("entitlement")?;
    if let Some(plan) = entitlement
        .get("subscription_plan")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(normalize_plan_type(plan));
    }
    if entitlement
        .get("has_active_subscription")
        .and_then(Value::as_bool)
        == Some(false)
    {
        return Some("free".to_string());
    }
    None
}

fn normalize_plan_type(value: &str) -> String {
    let lowered = value.trim().to_ascii_lowercase();
    if lowered.contains("team") {
        "team".to_string()
    } else if lowered.contains("pro") {
        "pro".to_string()
    } else if lowered.contains("plus") {
        "plus".to_string()
    } else if lowered.contains("enterprise") {
        "enterprise".to_string()
    } else if lowered.contains("free") {
        "free".to_string()
    } else {
        lowered
    }
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

async fn response_json_or_text(response: reqwest::Response) -> Option<Value> {
    let raw = response.text().await.ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(&raw)
        .ok()
        .or_else(|| Some(json!({ "raw": raw })))
}

fn compact_error_message(value: &Value) -> String {
    if let Some(message) =
        first_string(value, &["message", "detail", "reason", "error_description"])
    {
        return message;
    }
    match value.get("error") {
        Some(Value::String(message)) => message.clone(),
        Some(Value::Object(error)) => first_string(
            &Value::Object(error.clone()),
            &["message", "code", "type", "error"],
        )
        .unwrap_or_else(|| value.to_string()),
        _ => value.to_string(),
    }
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn chatgpt_account_id(auth_file: &CodexAuthFile) -> Option<&str> {
    auth_file
        .account_id
        .as_deref()
        .or(auth_file.chatgpt_account_id.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|_| {
            !auth_file
                .plan_type
                .as_deref()
                .is_some_and(|plan| plan.eq_ignore_ascii_case("free"))
        })
}

fn auth_index(auth_file: &CodexAuthFile) -> Option<String> {
    for key in ["auth_index", "authIndex"] {
        if let Some(value) = auth_file.extra.get(key) {
            match value {
                Value::String(text) => {
                    let text = text.trim();
                    if !text.is_empty() {
                        return Some(text.to_string());
                    }
                }
                Value::Number(number) => return Some(number.to_string()),
                _ => {}
            }
        }
    }
    None
}

fn cpa_base_url(settings: &AutoProbeSettings) -> Result<String, ApiError> {
    settings
        .cpa_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/').to_string())
        .ok_or_else(|| ApiError::bad_request("CPA base URL is required"))
}

fn cpa_probe_payload(auth_file: &CodexAuthFile, auth_index: &str) -> Value {
    let mut headers = serde_json::Map::new();
    headers.insert("Authorization".to_string(), json!("Bearer $TOKEN$"));
    headers.insert("Content-Type".to_string(), json!("application/json"));
    headers.insert("User-Agent".to_string(), json!(CPA_PROBE_USER_AGENT));
    if let Some(account_id) = chatgpt_account_id(auth_file) {
        headers.insert("Chatgpt-Account-Id".to_string(), json!(account_id));
    }
    json!({
        "authIndex": auth_index,
        "method": "GET",
        "url": CODEX_WHAM_USAGE_URL,
        "header": headers,
    })
}

fn cpa_header_map(management_key: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {management_key}")) {
        headers.insert(header::AUTHORIZATION, value);
    }
    if let Ok(value) = HeaderValue::from_str(management_key) {
        headers.insert(HeaderName::from_static("x-management-key"), value);
    }
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    headers
}

async fn cpa_list_auth_files(
    http: &Client,
    base_url: &str,
    management_key: &str,
) -> Result<Vec<Value>, ApiError> {
    let response = http
        .get(format!("{base_url}/v0/management/auth-files"))
        .headers(cpa_header_map(management_key))
        .send()
        .await
        .map_err(|error| {
            ApiError::bad_request(format!("CPA auth-files request failed: {error}"))
        })?;
    let status = response.status();
    let payload = response_json_or_text(response)
        .await
        .unwrap_or_else(|| json!({}));
    if !status.is_success() {
        return Err(ApiError::bad_request(format!(
            "CPA auth-files returned {status}: {}",
            compact_error_message(&payload)
        )));
    }
    let files = payload
        .get("files")
        .or_else(|| payload.get("data"))
        .or_else(|| payload.get("items"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(files)
}

async fn cpa_probe_item(
    http: &Client,
    base_url: &str,
    management_key: &str,
    item: &Value,
) -> Value {
    let auth_index = item
        .get("auth_index")
        .or_else(|| item.get("authIndex"))
        .and_then(|value| match value {
            Value::String(text) => Some(text.trim().to_string()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
        .unwrap_or_default();
    let mut result = json!({
        "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
        "email": item.get("email").or_else(|| item.get("account")).and_then(Value::as_str).unwrap_or_default(),
        "auth_index": auth_index,
        "status_code": null,
        "ok": false,
        "action": "probe_failed",
        "message": "",
    });
    if auth_index.is_empty() {
        result["action"] = json!("skipped");
        result["message"] = json!("missing auth_index");
        return result;
    }
    let auth_file = cpa_item_to_auth_file(item);
    let outcome = cpa_wham_probe(http, base_url, management_key, &auth_file, &auth_index).await;
    result["status_code"] = outcome.http_status.map(Value::from).unwrap_or(Value::Null);
    result["message"] = json!(outcome.error.unwrap_or_else(|| {
        outcome
            .body
            .as_ref()
            .map(compact_error_message)
            .unwrap_or_else(|| "ok".to_string())
    }));
    match outcome.http_status {
        Some(200) => {
            result["ok"] = json!(true);
            result["action"] = json!("ready");
        }
        Some(401) => result["action"] = json!("401"),
        Some(403) => result["action"] = json!("risk_blocked"),
        Some(429) => result["action"] = json!("usage_limit_reached"),
        Some(_) => result["action"] = json!("http_error"),
        None => result["action"] = json!("probe_failed"),
    }
    result
}

fn cpa_item_to_auth_file(item: &Value) -> CodexAuthFile {
    serde_json::from_value::<CodexAuthFile>(item.clone())
        .unwrap_or_default()
        .normalized()
}

fn cpa_payload_status_code(payload: &Value) -> Option<u16> {
    payload
        .get("status_code")
        .or_else(|| payload.get("statusCode"))
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .or_else(|| {
            payload
                .get("body")
                .and_then(Value::as_str)
                .and_then(|body| serde_json::from_str::<Value>(body).ok())
                .and_then(|body| {
                    body.get("status")
                        .and_then(Value::as_u64)
                        .and_then(|value| u16::try_from(value).ok())
                })
        })
}

fn cpa_payload_body(payload: &Value) -> Option<Value> {
    let body = payload.get("body")?;
    match body {
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .or_else(|| Some(json!({ "raw": text }))),
        other => Some(other.clone()),
    }
}

async fn resolve_probe_http_client(
    state: &AppState,
    settings: &AutoProbeSettings,
) -> Result<(Client, Option<String>), ApiError> {
    if !settings.proxy_enabled {
        return Ok((state.http.clone(), None));
    }
    let raw_proxy = if settings.proxy_mode == "api" {
        let api_url = settings
            .proxy_api_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::bad_request("proxy API URL is required"))?;
        fetch_dynamic_proxy(&state.http, api_url).await?
    } else {
        settings
            .proxy_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| ApiError::bad_request("proxy URL is required"))?
    };
    let proxy_url = normalize_proxy_url(&raw_proxy, &settings.proxy_default_scheme)?;
    let proxy = Proxy::all(&proxy_url)
        .map_err(|error| ApiError::bad_request(format!("invalid probe proxy: {error}")))?;
    let client = Client::builder()
        .user_agent("AetherPool/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .proxy(proxy)
        .build()
        .map_err(|error| ApiError::bad_request(format!("probe proxy client failed: {error}")))?;
    Ok((client, Some(redact_proxy_url(&proxy_url))))
}

async fn fetch_dynamic_proxy(http: &Client, api_url: &str) -> Result<String, ApiError> {
    let response = http
        .get(api_url)
        .header("accept", "application/json,text/plain,*/*")
        .send()
        .await
        .map_err(|error| ApiError::bad_request(format!("proxy API request failed: {error}")))?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        ApiError::bad_request(format!("proxy API response read failed: {error}"))
    })?;
    if !status.is_success() {
        return Err(ApiError::bad_request(format!(
            "proxy API returned {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    extract_proxy_from_api_body(&body)
        .ok_or_else(|| ApiError::bad_request("proxy API did not return a usable proxy"))
}

fn normalize_proxy_url(raw: &str, default_scheme: &str) -> Result<String, ApiError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(ApiError::bad_request("proxy URL is empty"));
    }
    if value.contains("://") {
        return Ok(value.to_string());
    }
    let scheme = match default_scheme.trim().to_ascii_lowercase().as_str() {
        "socks" | "socks5" => "socks5",
        "socks5h" => "socks5h",
        _ => "http",
    };
    if let Some((host, port, username, password)) = split_four_part_proxy(value) {
        return Ok(format!("{scheme}://{username}:{password}@{host}:{port}"));
    }
    Ok(format!("{scheme}://{value}"))
}

fn extract_proxy_from_api_body(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if let Some(proxy) = extract_proxy_from_json(&value) {
            return Some(proxy);
        }
    }
    extract_proxy_token(trimmed)
}

fn extract_proxy_from_json(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => extract_proxy_token(value),
        Value::Array(items) => items.iter().find_map(extract_proxy_from_json),
        Value::Object(object) => {
            for key in [
                "proxy",
                "proxy_url",
                "url",
                "http",
                "https",
                "socks5",
                "server",
                "address",
            ] {
                if let Some(proxy) = object.get(key).and_then(extract_proxy_from_json) {
                    return Some(proxy);
                }
            }
            let host = object
                .get("host")
                .or_else(|| object.get("ip"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let port = object
                .get("port")
                .and_then(|value| {
                    value
                        .as_u64()
                        .map(|port| port.to_string())
                        .or_else(|| value.as_str().map(|port| port.trim().to_string()))
                })
                .filter(|value| !value.is_empty());
            if let (Some(host), Some(port)) = (host, port) {
                return Some(format!("{host}:{port}"));
            }
            for key in ["data", "result", "list", "proxies"] {
                if let Some(proxy) = object.get(key).and_then(extract_proxy_from_json) {
                    return Some(proxy);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_proxy_token(text: &str) -> Option<String> {
    text.split(|character: char| character.is_whitespace() || character == ',' || character == ';')
        .map(str::trim)
        .map(|value| {
            value.trim_matches(|character| matches!(character, '"' | '\'' | '[' | ']' | '{' | '}'))
        })
        .find(|value| looks_like_proxy(value))
        .map(ToOwned::to_owned)
}

fn extract_ip_from_body(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if let Some(ip) = extract_ip_from_json(&value) {
            return Some(ip);
        }
    }
    extract_ip_token(trimmed)
}

fn extract_ip_from_json(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => extract_ip_token(value),
        Value::Array(items) => items.iter().find_map(extract_ip_from_json),
        Value::Object(object) => {
            for key in [
                "ip",
                "origin",
                "query",
                "client_ip",
                "remote_addr",
                "address",
            ] {
                if let Some(ip) = object.get(key).and_then(extract_ip_from_json) {
                    return Some(ip);
                }
            }
            object.values().find_map(extract_ip_from_json)
        }
        _ => None,
    }
}

fn extract_ip_token(text: &str) -> Option<String> {
    text.split(|character: char| {
        character.is_whitespace()
            || character == ','
            || character == ';'
            || character == '"'
            || character == '\''
    })
    .map(clean_ip_candidate)
    .find_map(|candidate| {
        candidate.parse::<IpAddr>().ok().or_else(|| {
            candidate
                .trim_matches(|character| matches!(character, '.' | ':'))
                .parse::<IpAddr>()
                .ok()
        })
    })
    .map(|ip| ip.to_string())
}

fn clean_ip_candidate(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(character, '[' | ']' | '{' | '}' | '(' | ')' | '<' | '>')
    })
}

fn looks_like_proxy(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value.contains("://") {
        return true;
    }
    if split_four_part_proxy(value).is_some() {
        return true;
    }
    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    !host.trim().is_empty() && port.trim().parse::<u16>().is_ok()
}

fn split_four_part_proxy(value: &str) -> Option<(&str, &str, &str, &str)> {
    let mut parts = value.splitn(4, ':');
    let host = parts.next()?.trim();
    let port = parts.next()?.trim();
    let username = parts.next()?.trim();
    let password = parts.next()?.trim();
    if host.is_empty() || username.is_empty() || password.is_empty() || port.parse::<u16>().is_err()
    {
        return None;
    }
    Some((host, port, username, password))
}

fn redact_proxy_url(value: &str) -> String {
    let Some(scheme_index) = value.find("://") else {
        return value.to_string();
    };
    let authority_start = scheme_index + 3;
    let after_authority = value[authority_start..]
        .find('/')
        .map(|index| authority_start + index)
        .unwrap_or(value.len());
    let authority = &value[authority_start..after_authority];
    let Some(at_index) = authority.rfind('@') else {
        return value.to_string();
    };
    format!(
        "{}***@{}{}",
        &value[..authority_start],
        &authority[at_index + 1..],
        &value[after_authority..]
    )
}

fn probe_run_payload(summary: &ProbeRunSummary, result_limit: usize) -> Value {
    json!({
        "success": summary.failed == 0,
        "checked": summary.checked,
        "failed": summary.failed,
        "results": summary.results.iter().take(result_limit).cloned().collect::<Vec<_>>(),
        "truncated": summary.results.len() > result_limit,
    })
}

fn auto_probe_settings_payload(settings: AutoProbeSettings) -> Value {
    let next_run_at = if settings.enabled {
        let base = settings
            .last_finished_at
            .or(settings.last_started_at)
            .unwrap_or_default();
        Some(if base == 0 {
            unix_now_secs()
        } else {
            base.saturating_add(settings.interval_seconds)
        })
    } else {
        None
    };
    json!({
        "success": true,
        "settings": settings,
        "next_run_at": next_run_at,
    })
}

fn redeem_rate_limit_settings_payload(settings: RedeemRateLimitSettings) -> Value {
    json!({
        "success": true,
        "settings": settings,
    })
}

#[derive(Default)]
struct RedeemRateLimiter {
    buckets: HashMap<String, RateLimitBucket>,
}

#[derive(Clone, Copy)]
struct RateLimitBucket {
    window_start: u64,
    count: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum RateLimitDecision {
    Allowed,
    Denied { retry_after_seconds: u64 },
}

impl RedeemRateLimiter {
    fn check(
        &mut self,
        key: &str,
        settings: &RedeemRateLimitSettings,
        now: u64,
    ) -> RateLimitDecision {
        let settings = settings.clone().normalized();
        if !settings.enabled || settings.max_requests == 0 {
            return RateLimitDecision::Allowed;
        }
        let window_seconds = settings.window_seconds.max(1);
        self.buckets
            .retain(|_, bucket| now < bucket.window_start.saturating_add(window_seconds * 2));
        let bucket = self
            .buckets
            .entry(key.to_string())
            .or_insert(RateLimitBucket {
                window_start: now,
                count: 0,
            });
        if now >= bucket.window_start.saturating_add(window_seconds) {
            bucket.window_start = now;
            bucket.count = 0;
        }
        if bucket.count >= settings.max_requests {
            return RateLimitDecision::Denied {
                retry_after_seconds: bucket
                    .window_start
                    .saturating_add(window_seconds)
                    .saturating_sub(now)
                    .max(1),
            };
        }
        bucket.count += 1;
        RateLimitDecision::Allowed
    }

    fn clear(&mut self) {
        self.buckets.clear();
    }
}

async fn enforce_redeem_rate_limit(
    state: &AppState,
    headers: &HeaderMap,
    peer_ip: IpAddr,
) -> Result<(), ApiError> {
    let settings = state.repo.get_redeem_rate_limit_settings().await?;
    if !settings.enabled {
        return Ok(());
    }
    let ip = redeem_rate_limit_client_ip(headers, peer_ip, state.trust_proxy_headers);
    if redeem_rate_limit_ip_whitelisted(&ip, &settings.whitelist_ips) {
        return Ok(());
    }
    match state
        .redeem_rate_limiter
        .lock()
        .await
        .check(&ip, &settings, unix_now_secs())
    {
        RateLimitDecision::Allowed => Ok(()),
        RateLimitDecision::Denied {
            retry_after_seconds,
        } => Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("兑换请求过于频繁，请 {retry_after_seconds} 秒后重试"),
        )),
    }
}

fn redeem_rate_limit_client_ip(
    headers: &HeaderMap,
    peer_ip: IpAddr,
    trust_proxy_headers: bool,
) -> String {
    if trust_proxy_headers {
        if let Some(ip) = client_ip_from_headers(headers) {
            return ip;
        }
    }
    peer_ip.to_string()
}

fn client_ip_from_headers(headers: &HeaderMap) -> Option<String> {
    for name in ["x-forwarded-for", "x-real-ip", "cf-connecting-ip"] {
        let Some(raw) = headers.get(name).and_then(|value| value.to_str().ok()) else {
            continue;
        };
        if let Some(ip) = raw.split(',').find_map(parse_header_ip) {
            return Some(ip);
        }
    }
    None
}

fn parse_header_ip(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_matches(['[', ']']);
    trimmed.parse::<IpAddr>().ok().map(|ip| ip.to_string())
}

fn redeem_rate_limit_ip_whitelisted(ip: &str, whitelist_ips: &[String]) -> bool {
    whitelist_ips
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .any(|value| value == ip)
}

#[derive(Default)]
struct RefreshOutcome {
    refreshed: usize,
    skipped: usize,
    failed: usize,
    results: Vec<Value>,
}

async fn refresh_expired_accounts(
    state: &AppState,
    account_ids: Option<&[String]>,
    pool_id: Option<&str>,
    force: bool,
) -> Result<RefreshOutcome, ApiError> {
    let now = unix_now_secs();
    let accounts = state
        .repo
        .load_unredeemed_auth_files_scoped(account_ids, pool_id)
        .await?;
    let settings = state.repo.get_auto_probe_settings().await?;
    let (refresh_http, refresh_proxy) = resolve_probe_http_client(state, &settings).await?;
    let mut outcome = RefreshOutcome::default();
    for (summary, auth_file) in accounts {
        let should_refresh = force
            || access_token_needs_refresh(
                auth_file.expires_at_epoch(),
                now,
                ACCESS_TOKEN_REFRESH_GRACE_SECONDS,
            );
        if !should_refresh {
            outcome.skipped += 1;
            continue;
        }
        match refresh_codex_auth_file_with_client(state, &refresh_http, &auth_file).await {
            Ok(refreshed) => {
                state
                    .repo
                    .update_account_auth(
                        &summary.id,
                        &refreshed,
                        AccountStatus::Available,
                        Some(unix_now_secs()),
                    )
                    .await?;
                outcome.refreshed += 1;
                outcome.results.push(json!({
                    "account_id": summary.id,
                    "status": "refreshed",
                    "proxy": refresh_proxy.clone()
                }));
            }
            Err(error) => {
                state
                    .repo
                    .mark_account_status(&summary.id, AccountStatus::RefreshFailed)
                    .await?;
                outcome.failed += 1;
                outcome.results.push(json!({
                    "account_id": summary.id,
                    "status": "refresh_failed",
                    "proxy": refresh_proxy.clone(),
                    "error": error
                }));
            }
        }
    }
    Ok(outcome)
}

async fn refresh_codex_auth_file_with_client(
    state: &AppState,
    http: &Client,
    auth_file: &CodexAuthFile,
) -> Result<CodexAuthFile, String> {
    let refresh_token = auth_file
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "missing refresh_token".to_string())?;
    refresh_openai_with_rt(state, http, auth_file, refresh_token).await
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    admin_request_authorized(state.admin_token.trim(), state.allow_open_admin, headers)
}

fn admin_request_authorized(
    expected: &str,
    allow_open_admin: bool,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    if expected.is_empty() {
        return if allow_open_admin {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "admin token is not configured",
            ))
        };
    }
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().strip_prefix("Bearer "))
        .map(str::trim);
    let token = bearer.or_else(|| {
        headers
            .get("x-admin-token")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    });
    if token.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes())) {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: String,
    }

    struct MockResponse {
        status: u16,
        body: String,
    }

    impl MockResponse {
        fn json(status: u16, body: Value) -> Self {
            Self {
                status,
                body: body.to_string(),
            }
        }
    }

    async fn spawn_mock_http(
        responses: Vec<MockResponse>,
    ) -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let records = Arc::new(Mutex::new(Vec::new()));
        let server_records = records.clone();
        tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_mock_request(&mut stream).await;
                server_records.lock().await.push(request);
                let reason = match response.status {
                    200 => "OK",
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    429 => "Too Many Requests",
                    _ => "Mock",
                };
                let raw_response = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    reason,
                    response.body.len(),
                    response.body
                );
                stream.write_all(raw_response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{address}"), records)
    }

    async fn read_mock_request(stream: &mut tokio::net::TcpStream) -> RecordedRequest {
        let mut buffer = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "mock connection closed before headers");
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(index) = find_bytes(&buffer, b"\r\n\r\n") {
                break index + 4;
            }
        };
        let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let mut lines = header_text.lines();
        let request_line = lines.next().unwrap_or_default();
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap_or_default().to_string();
        let path = request_parts.next().unwrap_or_default().to_string();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect::<HashMap<_, _>>();
        let content_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_default();
        while buffer.len() < header_end + content_length {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "mock connection closed before body");
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body =
            String::from_utf8_lossy(&buffer[header_end..header_end + content_length]).to_string();
        RecordedRequest {
            method,
            path,
            headers,
            body,
        }
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    async fn test_state(base_url: &str) -> AppState {
        let path = std::env::temp_dir().join(format!(
            "aether-pool-api-test-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let repo = AccountPoolRepository::connect(&format!("sqlite://{}", path.display()), "test")
            .await
            .unwrap();
        AppState {
            repo,
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            admin_token: Arc::new("admin".to_string()),
            allow_open_admin: false,
            oauth_client_id: Arc::new("client-id".to_string()),
            oauth_token_url: Arc::new(format!("{base_url}/oauth/token")),
            chatgpt_check_url: Arc::new(format!(
                "{base_url}/backend-api/accounts/check/v4-2023-04-27?timezone_offset_min=-480"
            )),
            chatgpt_session_url: Arc::new(format!("{base_url}/api/auth/session")),
            wham_usage_url: Arc::new(format!("{base_url}/backend-api/wham/usage")),
            ip_check_url: Arc::new(format!("{base_url}/ip")),
            trust_proxy_headers: false,
            auto_probe_lock: Arc::new(Mutex::new(())),
            redeem_rate_limiter: Arc::new(Mutex::new(RedeemRateLimiter::default())),
        }
    }

    #[test]
    fn empty_admin_token_is_locked_by_default() {
        let headers = HeaderMap::new();
        let error = admin_request_authorized("", false, &headers).unwrap_err();
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn explicit_open_admin_allows_empty_token() {
        let headers = HeaderMap::new();
        assert!(admin_request_authorized("", true, &headers).is_ok());
    }

    #[test]
    fn bearer_admin_token_is_required() {
        let mut headers = HeaderMap::new();
        assert!(admin_request_authorized("secret", false, &headers).is_err());
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert!(admin_request_authorized("secret", false, &headers).is_ok());
        assert!(admin_request_authorized("secret2", false, &headers).is_err());
    }

    #[test]
    fn normalizes_plain_proxy_with_default_scheme() {
        assert_eq!(
            normalize_proxy_url("127.0.0.1:1080", "socks").unwrap(),
            "socks5://127.0.0.1:1080"
        );
        assert_eq!(
            normalize_proxy_url("http://user:pass@example.com:10000", "socks5").unwrap(),
            "http://user:pass@example.com:10000"
        );
        assert_eq!(
            normalize_proxy_url("proxy.example:10000:user:pass", "http").unwrap(),
            "http://user:pass@proxy.example:10000"
        );
    }

    #[test]
    fn extracts_proxy_from_plain_and_json_api_body() {
        assert_eq!(
            extract_proxy_from_api_body("1.2.3.4:10000\n5.6.7.8:10000").as_deref(),
            Some("1.2.3.4:10000")
        );
        assert_eq!(
            extract_proxy_from_api_body(
                r#"{"code":0,"data":[{"host":"proxy.example","port":10000}]}"#
            )
            .as_deref(),
            Some("proxy.example:10000")
        );
        assert_eq!(
            extract_proxy_from_api_body(r#"{"data":["socks5://user:pass@127.0.0.1:1080"]}"#)
                .as_deref(),
            Some("socks5://user:pass@127.0.0.1:1080")
        );
        assert_eq!(
            extract_proxy_from_api_body("proxy.example:10000:user:pass").as_deref(),
            Some("proxy.example:10000:user:pass")
        );
    }

    #[test]
    fn extracts_ip_from_common_check_responses() {
        assert_eq!(
            extract_ip_from_body(r#"{"ip":"203.0.113.10"}"#).as_deref(),
            Some("203.0.113.10")
        );
        assert_eq!(
            extract_ip_from_body(r#"{"origin":"198.51.100.3, 198.51.100.4"}"#).as_deref(),
            Some("198.51.100.3")
        );
        assert_eq!(
            extract_ip_from_body("2001:db8::1").as_deref(),
            Some("2001:db8::1")
        );
    }

    #[test]
    fn redacts_proxy_credentials() {
        assert_eq!(
            redact_proxy_url("http://user:pass@example.com:10000"),
            "http://***@example.com:10000"
        );
        assert_eq!(
            redact_proxy_url("socks5://127.0.0.1:1080"),
            "socks5://127.0.0.1:1080"
        );
    }

    #[test]
    fn cpa_probe_payload_uses_gpt_account_manager_headers() {
        let auth_file = CodexAuthFile {
            account_id: Some("acct-1".to_string()),
            plan_type: Some("plus".to_string()),
            ..CodexAuthFile::default()
        };
        let payload = cpa_probe_payload(&auth_file, "7");
        assert_eq!(payload["authIndex"], "7");
        assert_eq!(payload["method"], "GET");
        assert_eq!(payload["url"], CODEX_WHAM_USAGE_URL);
        let header = payload["header"].as_object().unwrap();
        assert_eq!(header.len(), 4);
        assert_eq!(header["Authorization"], "Bearer $TOKEN$");
        assert_eq!(header["Content-Type"], "application/json");
        assert_eq!(header["User-Agent"], CPA_PROBE_USER_AGENT);
        assert_eq!(header["Chatgpt-Account-Id"], "acct-1");
        assert!(!header.contains_key("originator"));
        assert!(!header.contains_key("session_id"));
    }

    #[test]
    fn lifecycle_diagnosis_status_maps_to_pool_status_without_changing_redeem_semantics() {
        assert_eq!(lifecycle_account_status("active"), AccountStatus::Available);
        assert_eq!(
            lifecycle_account_status("refreshed"),
            AccountStatus::Available
        );
        assert_eq!(
            lifecycle_account_status("rt_rotated"),
            AccountStatus::Available
        );
        assert_eq!(
            lifecycle_account_status("usage_limit_reached"),
            AccountStatus::QuotaExhausted
        );
        assert_eq!(
            lifecycle_account_status("risk_blocked"),
            AccountStatus::Forbidden
        );
        assert_eq!(
            lifecycle_account_status("banned"),
            AccountStatus::AuthInvalid
        );
        assert_eq!(
            lifecycle_account_status("session_expired"),
            AccountStatus::AuthInvalid
        );
        assert_eq!(
            lifecycle_account_status("rt_invalid"),
            AccountStatus::AuthInvalid
        );
        assert_eq!(
            lifecycle_account_status("needs_login"),
            AccountStatus::AuthInvalid
        );
        assert_eq!(
            lifecycle_account_status("probe_failed"),
            AccountStatus::RefreshFailed
        );
    }

    #[tokio::test]
    async fn access_token_probe_uses_accounts_check_headers_and_maps_statuses() {
        let (base_url, records) = spawn_mock_http(vec![
            MockResponse::json(
                200,
                json!({
                    "accounts": {
                        "default": {
                            "entitlement": {
                                "subscription_plan": "chatgpt_plus"
                            }
                        }
                    }
                }),
            ),
            MockResponse::json(401, json!({ "error": "expired" })),
            MockResponse::json(403, json!({ "error": "banned" })),
            MockResponse::json(429, json!({ "detail": "usage_limit_reached" })),
        ])
        .await;
        let state = test_state(&base_url).await;

        let active = probe_openai_access_token(
            &state,
            &state.http,
            &CodexAuthFile {
                access_token: Some("at-active".to_string()),
                ..CodexAuthFile::default()
            },
        )
        .await;
        let expired = probe_openai_access_token(
            &state,
            &state.http,
            &CodexAuthFile {
                access_token: Some("at-expired".to_string()),
                ..CodexAuthFile::default()
            },
        )
        .await;
        let banned = probe_openai_access_token(
            &state,
            &state.http,
            &CodexAuthFile {
                access_token: Some("at-banned".to_string()),
                ..CodexAuthFile::default()
            },
        )
        .await;
        let quota = probe_openai_access_token(
            &state,
            &state.http,
            &CodexAuthFile {
                access_token: Some("at-quota".to_string()),
                ..CodexAuthFile::default()
            },
        )
        .await;

        assert_eq!(active.status, "active");
        assert_eq!(active.plan_type.as_deref(), Some("plus"));
        assert_eq!(expired.status, "session_expired");
        assert_eq!(banned.status, "banned");
        assert_eq!(quota.status, "usage_limit_reached");
        assert!(quota.credential_ok);
        assert!(!quota.usable);

        let records = records.lock().await;
        assert_eq!(records.len(), 4);
        let first = &records[0];
        assert_eq!(first.method, "GET");
        assert_eq!(
            first.path,
            "/backend-api/accounts/check/v4-2023-04-27?timezone_offset_min=-480"
        );
        assert_eq!(
            first.headers.get("authorization").map(String::as_str),
            Some("Bearer at-active")
        );
        assert_eq!(
            first.headers.get("user-agent").map(String::as_str),
            Some(OPENAI_BROWSER_USER_AGENT)
        );
        assert_eq!(
            first.headers.get("accept").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            first.headers.get("referer").map(String::as_str),
            Some("https://chatgpt.com/")
        );
        assert!(!first.headers.contains_key("sec-ch-ua"));
        assert!(first.body.is_empty());
    }

    #[tokio::test]
    async fn lifecycle_diagnosis_refreshes_in_order_and_uses_aligned_headers() {
        let (base_url, records) = spawn_mock_http(vec![
            MockResponse::json(
                200,
                json!({
                    "access_token": "at-new",
                    "refresh_token": "rt-new",
                    "id_token": "id-new",
                    "expires_in": 3600
                }),
            ),
            MockResponse::json(
                200,
                json!({
                    "accounts": {
                        "default": {
                            "entitlement": {
                                "has_active_subscription": false
                            }
                        }
                    }
                }),
            ),
            MockResponse::json(400, json!({ "error": "invalid_grant" })),
            MockResponse::json(401, json!({ "error": "session expired" })),
        ])
        .await;
        let state = test_state(&base_url).await;

        let refreshed = diagnose_lifecycle(
            &state,
            &state.http,
            &CodexAuthFile {
                refresh_token: Some("rt-old".to_string()),
                access_token: Some("at-old".to_string()),
                ..CodexAuthFile::default()
            },
        )
        .await;
        assert_eq!(refreshed.status, "rt_rotated");
        assert!(refreshed.access_token_updated);
        assert!(refreshed.refresh_token_rotated);
        let refreshed_auth = refreshed.auth_file.as_ref().unwrap();
        assert_eq!(refreshed_auth.access_token.as_deref(), Some("at-new"));
        assert_eq!(refreshed_auth.refresh_token.as_deref(), Some("rt-new"));

        let rt_invalid = diagnose_lifecycle(
            &state,
            &state.http,
            &CodexAuthFile {
                refresh_token: Some("bad-rt".to_string()),
                ..CodexAuthFile::default()
            },
        )
        .await;
        assert_eq!(rt_invalid.status, "rt_invalid");

        let session_expired = diagnose_lifecycle(
            &state,
            &state.http,
            &CodexAuthFile {
                session_token: Some("session-token".to_string()),
                ..CodexAuthFile::default()
            },
        )
        .await;
        assert_eq!(session_expired.status, "session_expired");

        let records = records.lock().await;
        assert_eq!(records.len(), 4);
        let refresh = &records[0];
        assert_eq!(refresh.method, "POST");
        assert_eq!(refresh.path, "/oauth/token");
        assert_eq!(
            refresh.headers.get("user-agent").map(String::as_str),
            Some(OPENAI_BROWSER_USER_AGENT)
        );
        assert_eq!(
            refresh.headers.get("accept").map(String::as_str),
            Some("application/json")
        );
        assert!(refresh.body.contains("grant_type=refresh_token"));
        assert!(refresh.body.contains("client_id=client-id"));
        assert!(refresh.body.contains("refresh_token=rt-old"));
        assert!(
            refresh.body.contains("scope=openid+profile+email")
                || refresh.body.contains("scope=openid%20profile%20email")
        );

        let check = &records[1];
        assert_eq!(check.method, "GET");
        assert_eq!(
            check.headers.get("authorization").map(String::as_str),
            Some("Bearer at-new")
        );

        let invalid_refresh = &records[2];
        assert_eq!(invalid_refresh.method, "POST");
        assert!(invalid_refresh.body.contains("refresh_token=bad-rt"));

        let session = &records[3];
        assert_eq!(session.method, "GET");
        assert_eq!(session.path, "/api/auth/session");
        assert_eq!(
            session.headers.get("user-agent").map(String::as_str),
            Some(OPENAI_BROWSER_USER_AGENT)
        );
        assert_eq!(
            session.headers.get("accept").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            session.headers.get("referer").map(String::as_str),
            Some("https://chatgpt.com/")
        );
        assert_eq!(
            session.headers.get("cookie").map(String::as_str),
            Some(
                "__Secure-next-auth.session-token=session-token; __Secure-authjs.session-token=session-token"
            )
        );
    }

    #[tokio::test]
    async fn cpa_wham_probe_posts_aligned_api_call_payload() {
        let (base_url, records) = spawn_mock_http(vec![MockResponse::json(
            200,
            json!({
                "status_code": 200,
                "body": "{\"accounts\":{\"default\":{\"account_id\":\"acct-1\"}}}"
            }),
        )])
        .await;
        let auth_file = CodexAuthFile {
            access_token: Some("at-cpa".to_string()),
            account_id: Some("acct-1".to_string()),
            plan_type: Some("plus".to_string()),
            ..CodexAuthFile::default()
        };

        let outcome =
            cpa_wham_probe(&Client::new(), &base_url, "management-key", &auth_file, "9").await;

        assert_eq!(outcome.source, "cpa_api_call");
        assert_eq!(outcome.http_status, Some(200));
        assert_eq!(outcome.result.status, AccountStatus::Available);

        let records = records.lock().await;
        assert_eq!(records.len(), 1);
        let request = &records[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v0/management/api-call");
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer management-key")
        );
        assert_eq!(
            request.headers.get("x-management-key").map(String::as_str),
            Some("management-key")
        );
        let body = serde_json::from_str::<Value>(&request.body).unwrap();
        assert_eq!(body["authIndex"], "9");
        assert_eq!(body["method"], "GET");
        assert_eq!(body["url"], CODEX_WHAM_USAGE_URL);
        let call_headers = body["header"].as_object().unwrap();
        assert_eq!(call_headers.len(), 4);
        assert_eq!(call_headers["Authorization"], "Bearer $TOKEN$");
        assert_eq!(call_headers["Content-Type"], "application/json");
        assert_eq!(call_headers["User-Agent"], CPA_PROBE_USER_AGENT);
        assert_eq!(call_headers["Chatgpt-Account-Id"], "acct-1");
    }

    #[test]
    fn startup_rejects_placeholder_secret_without_dev_override() {
        assert!(validate_startup_secrets(
            "real-admin-token",
            "change-this-long-random-secret",
            false,
            false,
        )
        .is_err());
        assert!(validate_startup_secrets(
            "real-admin-token",
            "change-this-long-random-secret",
            false,
            true,
        )
        .is_ok());
    }

    #[test]
    fn startup_rejects_placeholder_admin_token_without_dev_override() {
        assert!(validate_startup_secrets(
            "change-this-admin-password",
            "real-secret-key",
            false,
            false,
        )
        .is_err());
        assert!(validate_startup_secrets(
            "change-this-admin-password",
            "real-secret-key",
            false,
            true,
        )
        .is_ok());
    }

    #[test]
    fn redeem_export_limits_reject_empty_or_too_large_requests() {
        assert!(validate_redeem_export_limits(0, 0).is_err());
        assert!(validate_redeem_export_limits(MAX_REDEEM_CODES_PER_REQUEST + 1, 0).is_err());
        assert!(validate_redeem_export_limits(1, MAX_REDEEM_ACCOUNTS_PER_REQUEST + 1).is_err());
        assert!(validate_redeem_export_limits(
            MAX_REDEEM_CODES_PER_REQUEST,
            MAX_REDEEM_ACCOUNTS_PER_REQUEST
        )
        .is_ok());
    }

    #[test]
    fn extracts_client_ip_from_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-forwarded-for"),
            HeaderValue::from_static("203.0.113.10, 10.0.0.2"),
        );
        assert_eq!(
            client_ip_from_headers(&headers).as_deref(),
            Some("203.0.113.10")
        );
        assert!(redeem_rate_limit_ip_whitelisted(
            "203.0.113.10",
            &["203.0.113.10".to_string()]
        ));
    }

    #[test]
    fn redeem_rate_limit_uses_peer_ip_unless_proxy_headers_are_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-forwarded-for"),
            HeaderValue::from_static("203.0.113.10, 10.0.0.2"),
        );
        let peer_ip = "198.51.100.20".parse::<IpAddr>().unwrap();

        assert_eq!(
            redeem_rate_limit_client_ip(&headers, peer_ip, false),
            "198.51.100.20"
        );
        assert_eq!(
            redeem_rate_limit_client_ip(&headers, peer_ip, true),
            "203.0.113.10"
        );
    }

    #[test]
    fn redeem_rate_limiter_denies_after_window_budget() {
        let settings = RedeemRateLimitSettings {
            enabled: true,
            window_seconds: 60,
            max_requests: 2,
            whitelist_ips: Vec::new(),
            updated_at: 0,
        };
        let mut limiter = RedeemRateLimiter::default();
        assert_eq!(
            limiter.check("203.0.113.10", &settings, 100),
            RateLimitDecision::Allowed
        );
        assert_eq!(
            limiter.check("203.0.113.10", &settings, 101),
            RateLimitDecision::Allowed
        );
        assert_eq!(
            limiter.check("203.0.113.10", &settings, 102),
            RateLimitDecision::Denied {
                retry_after_seconds: 58
            }
        );
        assert_eq!(
            limiter.check("203.0.113.10", &settings, 160),
            RateLimitDecision::Allowed
        );
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl From<DataError> for ApiError {
    fn from(error: DataError) -> Self {
        if let DataError::InvalidInput(message) = error {
            return Self::bad_request(message);
        }
        tracing::error!(error = %error, "data layer error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "success": false,
                "error": self.message,
            })),
        )
            .into_response()
    }
}
