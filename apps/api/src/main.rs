use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use account_pool_core::{
    access_token_needs_refresh, export_cpa_zip_from_document, normalize_wham_usage_response,
    parse_codex_accounts, unix_now_secs, AccountStatus, CodexAuthFile, ExportFormat,
    ACCESS_TOKEN_REFRESH_GRACE_SECONDS, CODEX_WHAM_USAGE_URL,
};
use account_pool_data::{
    AccountListQuery, AccountPoolRepository, AccountSummary, AutoProbeSettings,
    CreateRedeemBatchInput, DataError,
};
use axum::extract::{Path, Query, State};
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

#[derive(Clone)]
struct AppState {
    repo: AccountPoolRepository,
    http: Client,
    admin_token: Arc<String>,
    allow_open_admin: bool,
    oauth_client_id: Arc<String>,
    oauth_token_url: Arc<String>,
    auto_probe_lock: Arc<Mutex<()>>,
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
        auto_probe_lock: Arc::new(Mutex::new(())),
    };

    let _auto_probe_worker = spawn_auto_probe_worker(state.clone());

    let app = router(state)
        .layer(TraceLayer::new_for_http())
        .layer(cors_layer());

    let addr: SocketAddr = std::env::var("AETHER_POOL_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8318".to_string())
        .parse()?;
    tracing::info!(%addr, "starting AetherPool API");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
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
        .route("/api/admin/accounts/import", post(import_accounts))
        .route("/api/admin/accounts", get(list_accounts))
        .route("/api/admin/accounts/probe", post(probe_accounts))
        .route("/api/admin/accounts/refresh", post(refresh_accounts))
        .route("/api/admin/accounts/export", post(export_admin_accounts))
        .route(
            "/api/admin/settings/auto-probe",
            get(get_auto_probe_settings).post(update_auto_probe_settings),
        )
        .route(
            "/api/admin/settings/auto-probe/run",
            post(run_auto_probe_once),
        )
        .route(
            "/api/admin/redeem-code-batches",
            post(create_redeem_batch).get(list_redeem_batches),
        )
        .route(
            "/api/admin/redeem-code-batches/{batch_id}/codes",
            get(list_redeem_codes),
        )
        .route("/api/redeem/export", post(redeem_export))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "aether-pool-api"
    }))
}

#[derive(Debug, Deserialize)]
struct ImportAccountsRequest {
    credentials: String,
}

async fn import_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ImportAccountsRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let parsed = parse_codex_accounts(&payload.credentials);
    let outcome = state.repo.import_accounts(&parsed.accounts).await?;
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
}

async fn refresh_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AccountIdRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let outcome = refresh_expired_accounts(&state, payload.account_ids.as_deref(), true).await?;
    Ok(Json(json!({
        "success": true,
        "refreshed": outcome.refreshed,
        "skipped": outcome.skipped,
        "failed": outcome.failed,
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
            refresh_before_probe: true,
            include_redeemed: payload.account_ids.is_some(),
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
    let settings = state.repo.save_auto_probe_settings(&settings).await?;
    Ok(Json(auto_probe_settings_payload(settings)))
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
            refresh_before_probe: settings.refresh_before_probe,
            include_redeemed: false,
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
        let _ = refresh_expired_accounts(&state, payload.account_ids.as_deref(), false).await?;
    }
    let document = state
        .repo
        .export_admin_accounts(payload.account_ids.as_deref(), include_redeemed, format)
        .await?;
    let download = export_download(format, &document, "aether-pool-admin");
    Ok(Json(json!({
        "success": true,
        "format": format.as_str(),
        "document": document,
        "download": download,
    })))
}

async fn create_redeem_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateRedeemBatchInput>,
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
    let outcome = state.repo.create_redeem_batch(payload).await?;
    Ok(Json(json!({
        "success": true,
        "batch_id": outcome.batch_id,
        "codes": outcome.codes,
    })))
}

async fn list_redeem_batches(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let items = state.repo.list_redeem_batches().await?;
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

async fn redeem_export(
    State(state): State<AppState>,
    Json(payload): Json<RedeemExportRequest>,
) -> Result<Json<Value>, ApiError> {
    if payload.codes.is_empty() {
        return Err(ApiError::bad_request("codes is required"));
    }
    let format = payload
        .format
        .parse::<ExportFormat>()
        .map_err(|_| ApiError::bad_request("unsupported export format"))?;
    let _ = refresh_expired_accounts(&state, None, false).await?;
    let outcome = state
        .repo
        .redeem_codes_for_export(&payload.codes, format)
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
            refresh_before_probe: settings.refresh_before_probe,
            include_redeemed: false,
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

#[derive(Clone, Copy)]
struct ProbeRunOptions {
    max_accounts: Option<usize>,
    concurrency: usize,
    refresh_before_probe: bool,
    include_redeemed: bool,
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
            state.repo.load_unredeemed_auth_files(None).await?
        }
    } else {
        state.repo.load_unredeemed_auth_files(account_ids).await?
    };
    if let Some(max_accounts) = options.max_accounts {
        accounts.truncate(max_accounts);
    }
    let probe_settings = state.repo.get_auto_probe_settings().await?;
    let (probe_http, probe_proxy) = resolve_probe_http_client(state, &probe_settings).await?;
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
        join_set.spawn(async move {
            probe_one_account(
                state,
                account,
                auth_file,
                options.refresh_before_probe,
                probe_http,
                probe_proxy,
            )
            .await
        });
    }
    while !join_set.is_empty() {
        collect_probe_result(&mut join_set, &mut summary).await;
    }
    Ok(summary)
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
    mut auth_file: CodexAuthFile,
    refresh_before_probe: bool,
    probe_http: Client,
    probe_proxy: Option<String>,
) -> Result<Value, ApiError> {
    let mut refresh_status = "skipped";
    if refresh_before_probe
        && summary.redeemed_at.is_none()
        && access_token_needs_refresh(
            auth_file.expires_at_epoch(),
            unix_now_secs(),
            ACCESS_TOKEN_REFRESH_GRACE_SECONDS,
        )
    {
        match refresh_codex_auth_file_with_client(&state, &probe_http, &auth_file).await {
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
                auth_file = refreshed;
                refresh_status = "refreshed";
            }
            Err(error) => {
                state
                    .repo
                    .mark_account_status(&summary.id, AccountStatus::RefreshFailed)
                    .await?;
                return Ok(json!({
                    "account_id": summary.id,
                    "status": AccountStatus::RefreshFailed.as_str(),
                    "refresh": "failed",
                    "proxy": probe_proxy,
                    "error": error,
                }));
            }
        }
    } else if refresh_before_probe && summary.redeemed_at.is_some() {
        refresh_status = "skipped_redeemed";
    }

    let started = Instant::now();
    let Some(access_token) = auth_file
        .access_token
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        state
            .repo
            .mark_account_status(&summary.id, AccountStatus::AuthInvalid)
            .await?;
        return Ok(json!({
            "account_id": summary.id,
            "status": AccountStatus::AuthInvalid.as_str(),
            "refresh": refresh_status,
            "proxy": probe_proxy,
            "error": "missing access_token"
        }));
    };
    let mut request = probe_http
        .get(CODEX_WHAM_USAGE_URL)
        .header("accept", "application/json")
        .bearer_auth(access_token);
    if let Some(account_id) = auth_file
        .account_id
        .as_ref()
        .or(auth_file.chatgpt_account_id.as_ref())
        .filter(|_| auth_file.plan_type.as_deref() != Some("free"))
    {
        request = request.header("chatgpt-account-id", account_id);
    }
    let (status_code, body, error) = match request.send().await {
        Ok(response) => {
            let status_code = response.status().as_u16();
            let body = response.json::<Value>().await.ok();
            (Some(status_code), body, None)
        }
        Err(error) => (None, None, Some(error.to_string())),
    };
    let mut result = if let Some(status_code) = status_code {
        normalize_wham_usage_response(status_code, body)
    } else {
        account_pool_core::HealthCheckResult {
            status: AccountStatus::RefreshFailed,
            plan_type: None,
            quota_snapshot: None,
            error,
        }
    };
    if result.plan_type.is_none() {
        result.plan_type = auth_file.plan_type.clone();
    }
    state
        .repo
        .record_health_check(
            &summary.id,
            &result,
            status_code,
            Some(started.elapsed().as_millis() as u64),
        )
        .await?;
    Ok(json!({
        "account_id": summary.id,
        "status": result.status.as_str(),
        "plan_type": result.plan_type,
        "http_status": status_code,
        "refresh": refresh_status,
        "proxy": probe_proxy,
        "error": result.error,
    }))
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
    force: bool,
) -> Result<RefreshOutcome, ApiError> {
    let now = unix_now_secs();
    let accounts = state.repo.load_unredeemed_auth_files(account_ids).await?;
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
        match refresh_codex_auth_file(state, &auth_file).await {
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
                    "status": "refreshed"
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
                    "error": error
                }));
            }
        }
    }
    Ok(outcome)
}

async fn refresh_codex_auth_file(
    state: &AppState,
    auth_file: &CodexAuthFile,
) -> Result<CodexAuthFile, String> {
    refresh_codex_auth_file_with_client(state, &state.http, auth_file).await
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
    let response = http
        .post(state.oauth_token_url.as_str())
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", state.oauth_client_id.as_str()),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .map_err(|error| format!("token response parse failed: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "token refresh failed ({status}): {}",
            body.get("error")
                .and_then(Value::as_str)
                .or_else(|| body.pointer("/error/message").and_then(Value::as_str))
                .unwrap_or("unknown error")
        ));
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
    if token == Some(expected) {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
