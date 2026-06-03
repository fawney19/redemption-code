use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use account_pool_core::{
    access_token_needs_refresh, export_cpa_zip_from_document, normalize_wham_usage_response,
    parse_codex_accounts, unix_now_secs, AccountStatus, CodexAuthFile, ExportFormat,
    ACCESS_TOKEN_REFRESH_GRACE_SECONDS, CODEX_WHAM_USAGE_URL,
};
use account_pool_data::{
    AccountListQuery, AccountPoolRepository, CreateRedeemBatchInput, DataError,
};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
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
    };

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
    let _ = refresh_expired_accounts(&state, payload.account_ids.as_deref(), false).await?;
    let accounts = state
        .repo
        .load_unredeemed_auth_files(payload.account_ids.as_deref())
        .await?;
    let mut results = Vec::new();
    for (summary, auth_file) in accounts {
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
            results.push(json!({
                "account_id": summary.id,
                "status": AccountStatus::AuthInvalid.as_str(),
                "error": "missing access_token"
            }));
            continue;
        };
        let mut request = state
            .http
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
        results.push(json!({
            "account_id": summary.id,
            "status": result.status.as_str(),
            "plan_type": result.plan_type,
            "http_status": status_code,
            "error": result.error,
        }));
    }
    Ok(Json(json!({
        "success": true,
        "results": results,
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
    let refresh_token = auth_file
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "missing refresh_token".to_string())?;
    let response = state
        .http
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
