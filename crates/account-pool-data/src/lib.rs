use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use account_pool_core::{
    access_token_needs_refresh, export_accounts, fingerprint_auth_file, format_redeem_code,
    generate_redeem_code, legacy_fingerprint_auth_file, mask_redeem_code, normalize_redeem_code,
    redeem_code_hash, secret_preview, unix_now_secs, AccountStatus, CodexAuthFile, ExportFormat,
    HealthCheckResult, ParsedAccount, ACCESS_TOKEN_REFRESH_GRACE_SECONDS,
};
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

const INIT_SQL: &str = include_str!("../migrations/sqlite/0001_init.sql");
const AUTO_PROBE_SETTINGS_KEY: &str = "auto_probe";

#[derive(Debug, Error)]
pub enum DataError {
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("encryption error")]
    Encryption,
    #[error("invalid export format")]
    InvalidExportFormat,
    #[error("not found")]
    NotFound,
}

#[derive(Clone)]
pub struct SecretBox {
    key: [u8; 32],
}

impl SecretBox {
    pub fn new(secret: &str) -> Self {
        let secret = if secret.trim().is_empty() {
            "aether-pool-local-development-secret"
        } else {
            secret.trim()
        };
        let digest = Sha256::digest(secret.as_bytes());
        let mut key = [0_u8; 32];
        key.copy_from_slice(&digest);
        Self { key }
    }

    pub fn encrypt_json<T: Serialize>(&self, value: &T) -> Result<String, DataError> {
        let plaintext = serde_json::to_vec(value).map_err(|_| DataError::Encryption)?;
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|_| DataError::Encryption)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_ref())
            .map_err(|_| DataError::Encryption)?;
        Ok(format!(
            "v1:{}:{}",
            STANDARD_NO_PAD.encode(nonce),
            STANDARD_NO_PAD.encode(ciphertext)
        ))
    }

    pub fn decrypt_json<T: for<'de> Deserialize<'de>>(
        &self,
        ciphertext: &str,
    ) -> Result<T, DataError> {
        let mut parts = ciphertext.split(':');
        let version = parts.next().unwrap_or_default();
        let nonce = parts.next().unwrap_or_default();
        let body = parts.next().unwrap_or_default();
        if version != "v1" || nonce.is_empty() || body.is_empty() {
            return Err(DataError::Encryption);
        }
        let nonce = STANDARD_NO_PAD
            .decode(nonce)
            .map_err(|_| DataError::Encryption)?;
        let body = STANDARD_NO_PAD
            .decode(body)
            .map_err(|_| DataError::Encryption)?;
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|_| DataError::Encryption)?;
        let plaintext = cipher
            .decrypt(Nonce::from_slice(&nonce), body.as_ref())
            .map_err(|_| DataError::Encryption)?;
        serde_json::from_slice(&plaintext).map_err(|_| DataError::Encryption)
    }
}

#[derive(Clone)]
pub struct AccountPoolRepository {
    pool: SqlitePool,
    secrets: SecretBox,
    redemption_lock: Arc<Mutex<()>>,
}

impl AccountPoolRepository {
    pub async fn connect(database_url: &str, secret_key: &str) -> Result<Self, DataError> {
        if let Some(path) = database_url.strip_prefix("sqlite://") {
            if path != ":memory:" {
                if let Some(parent) = Path::new(path).parent() {
                    std::fs::create_dir_all(parent).map_err(|_| DataError::Encryption)?;
                }
            }
        }
        let options = SqliteConnectOptions::from_str(database_url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        for statement in INIT_SQL.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            sqlx::query(statement).execute(&pool).await?;
        }
        ensure_schema_upgrades(&pool).await?;
        Ok(Self {
            pool,
            secrets: SecretBox::new(secret_key),
            redemption_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn import_accounts(
        &self,
        accounts: &[ParsedAccount],
    ) -> Result<ImportAccountsOutcome, DataError> {
        let now = unix_now_secs() as i64;
        let mut imported = 0;
        let mut updated = 0;
        for parsed in accounts {
            let auth_file = parsed.auth_file.clone().normalized();
            let fingerprint = fingerprint_auth_file(&auth_file);
            let exists = self
                .find_import_existing_account(&auth_file, &fingerprint)
                .await?;
            let ciphertext = self.secrets.encrypt_json(&auth_file)?;
            let expires_at = auth_file.expires_at_epoch().map(|value| value as i64);
            let status = if expires_at.is_some_and(|value| value <= now) {
                AccountStatus::AtExpired
            } else {
                AccountStatus::Available
            };
            if let Some(row) = exists {
                let id: String = row.try_get("id")?;
                sqlx::query(
                    r#"
UPDATE accounts
SET email = ?, name = ?, account_id = ?, plan_type = ?, auth_fingerprint = ?,
    auth_file_ciphertext = CASE WHEN redeemed_at IS NULL THEN ? ELSE auth_file_ciphertext END,
    access_token_preview = CASE WHEN redeemed_at IS NULL THEN ? ELSE access_token_preview END,
    refresh_token_preview = CASE WHEN redeemed_at IS NULL THEN ? ELSE refresh_token_preview END,
    expires_at = CASE WHEN redeemed_at IS NULL THEN ? ELSE expires_at END,
    status = CASE WHEN redeemed_at IS NULL THEN ? ELSE status END,
    updated_at = ?
WHERE id = ?
"#,
                )
                .bind(&auth_file.email)
                .bind(&auth_file.name)
                .bind(
                    auth_file
                        .account_id
                        .or(auth_file.chatgpt_account_id.clone()),
                )
                .bind(auth_file.plan_type.or(auth_file.chatgpt_plan_type.clone()))
                .bind(&fingerprint)
                .bind(ciphertext)
                .bind(secret_preview(auth_file.access_token.as_deref()))
                .bind(secret_preview(auth_file.refresh_token.as_deref()))
                .bind(expires_at)
                .bind(status.as_str())
                .bind(now)
                .bind(id)
                .execute(&self.pool)
                .await?;
                updated += 1;
            } else {
                sqlx::query(
                    r#"
INSERT INTO accounts (
  id, email, name, account_id, plan_type, status, auth_fingerprint,
  auth_file_ciphertext, access_token_preview, refresh_token_preview,
  expires_at, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
                )
                .bind(Uuid::new_v4().to_string())
                .bind(&auth_file.email)
                .bind(&auth_file.name)
                .bind(
                    auth_file
                        .account_id
                        .or(auth_file.chatgpt_account_id.clone()),
                )
                .bind(auth_file.plan_type.or(auth_file.chatgpt_plan_type.clone()))
                .bind(status.as_str())
                .bind(fingerprint)
                .bind(ciphertext)
                .bind(secret_preview(auth_file.access_token.as_deref()))
                .bind(secret_preview(auth_file.refresh_token.as_deref()))
                .bind(expires_at)
                .bind(now)
                .bind(now)
                .execute(&self.pool)
                .await?;
                imported += 1;
            }
        }
        Ok(ImportAccountsOutcome { imported, updated })
    }

    async fn find_import_existing_account(
        &self,
        auth_file: &CodexAuthFile,
        fingerprint: &str,
    ) -> Result<Option<sqlx::sqlite::SqliteRow>, DataError> {
        let current = sqlx::query("SELECT id FROM accounts WHERE auth_fingerprint = ?")
            .bind(fingerprint)
            .fetch_optional(&self.pool)
            .await?;
        if current.is_some() {
            return Ok(current);
        }

        let Some(email) = auth_file
            .email
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase)
        else {
            return Ok(None);
        };
        let legacy_fingerprint = legacy_fingerprint_auth_file(auth_file);
        if legacy_fingerprint == fingerprint {
            return Ok(None);
        }

        sqlx::query(
            r#"
SELECT id
FROM accounts
WHERE auth_fingerprint = ? AND lower(email) = ?
"#,
        )
        .bind(legacy_fingerprint)
        .bind(email)
        .fetch_optional(&self.pool)
        .await
        .map_err(DataError::from)
    }

    pub async fn list_accounts(
        &self,
        query: AccountListQuery,
    ) -> Result<AccountListPage, DataError> {
        let rows = sqlx::query(
            r#"
SELECT a.id, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at
FROM accounts a
LEFT JOIN redeem_codes rc ON rc.id = a.redeem_code_id
ORDER BY a.updated_at DESC, a.created_at DESC
"#,
        )
        .fetch_all(&self.pool)
        .await?;
        let search = query.search.map(|value| value.to_ascii_lowercase());
        let mut items = rows
            .into_iter()
            .map(|row| account_summary_from_row(&row))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|item| {
                if let Some(search) = search.as_ref() {
                    let haystack = format!(
                        "{} {} {} {}",
                        item.email.as_deref().unwrap_or_default(),
                        item.name.as_deref().unwrap_or_default(),
                        item.account_id.as_deref().unwrap_or_default(),
                        item.plan_type.as_deref().unwrap_or_default()
                    )
                    .to_ascii_lowercase();
                    if !haystack.contains(search) {
                        return false;
                    }
                }
                if let Some(status) = query.status.as_ref() {
                    if item.status != *status {
                        return false;
                    }
                }
                if let Some(redeemed) = query.redeemed {
                    if item.redeemed_at.is_some() != redeemed {
                        return false;
                    }
                }
                true
            })
            .collect::<Vec<_>>();
        let total = items.len();
        let offset = query.offset.min(total);
        let limit = query.limit.clamp(1, 500);
        items = items.into_iter().skip(offset).take(limit).collect();
        Ok(AccountListPage {
            items,
            total,
            limit,
            offset,
        })
    }

    pub async fn delete_unbound_accounts(
        &self,
        ids: &[String],
    ) -> Result<DeleteAccountsOutcome, DataError> {
        let mut outcome = DeleteAccountsOutcome::default();
        let mut tx = self.pool.begin().await?;

        for account_id in ids
            .iter()
            .map(|value| value.trim())
            .filter(|id| !id.is_empty())
        {
            let Some(row) = sqlx::query(
                r#"
SELECT redeem_code_id, redemption_id, redeemed_at
FROM accounts
WHERE id = ?
"#,
            )
            .bind(account_id)
            .fetch_optional(&mut *tx)
            .await?
            else {
                outcome.not_found += 1;
                outcome.results.push(DeleteAccountResult {
                    account_id: account_id.to_string(),
                    status: "not_found".to_string(),
                    reason: Some("账号不存在".to_string()),
                });
                continue;
            };

            let redeem_code_id: Option<String> = row.try_get("redeem_code_id")?;
            let redemption_id: Option<String> = row.try_get("redemption_id")?;
            let redeemed_at: Option<i64> = row.try_get("redeemed_at")?;
            if redeem_code_id.is_some() || redemption_id.is_some() || redeemed_at.is_some() {
                outcome.skipped += 1;
                outcome.results.push(DeleteAccountResult {
                    account_id: account_id.to_string(),
                    status: "skipped".to_string(),
                    reason: Some("账号已绑定兑换码或已兑换".to_string()),
                });
                continue;
            }

            let deleted = sqlx::query(
                r#"
DELETE FROM accounts
WHERE id = ? AND redeem_code_id IS NULL AND redemption_id IS NULL AND redeemed_at IS NULL
"#,
            )
            .bind(account_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if deleted == 1 {
                outcome.deleted += 1;
                outcome.results.push(DeleteAccountResult {
                    account_id: account_id.to_string(),
                    status: "deleted".to_string(),
                    reason: None,
                });
            } else {
                outcome.skipped += 1;
                outcome.results.push(DeleteAccountResult {
                    account_id: account_id.to_string(),
                    status: "skipped".to_string(),
                    reason: Some("账号状态已变化，未删除".to_string()),
                });
            }
        }

        tx.commit().await?;
        Ok(outcome)
    }

    pub async fn load_auth_files_for_ids(
        &self,
        ids: &[String],
        include_redeemed: bool,
    ) -> Result<Vec<(AccountSummary, CodexAuthFile)>, DataError> {
        let mut out = Vec::new();
        for id in ids {
            let row = sqlx::query(
                r#"
SELECT a.id, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at, a.auth_file_ciphertext
FROM accounts a
LEFT JOIN redeem_codes rc ON rc.id = a.redeem_code_id
WHERE a.id = ? AND (? = 1 OR a.redeemed_at IS NULL)
"#,
            )
            .bind(id)
            .bind(if include_redeemed { 1_i64 } else { 0_i64 })
            .fetch_optional(&self.pool)
            .await?;
            if let Some(row) = row {
                out.push(self.auth_pair_from_row(row)?);
            }
        }
        Ok(out)
    }

    pub async fn load_unredeemed_auth_files(
        &self,
        ids: Option<&[String]>,
    ) -> Result<Vec<(AccountSummary, CodexAuthFile)>, DataError> {
        if let Some(ids) = ids {
            return self.load_auth_files_for_ids(ids, false).await;
        }
        let rows = sqlx::query(
            r#"
SELECT a.id, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at, a.auth_file_ciphertext
FROM accounts a
LEFT JOIN redeem_codes rc ON rc.id = a.redeem_code_id
WHERE a.redeemed_at IS NULL
ORDER BY a.created_at ASC
"#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| self.auth_pair_from_row(row))
            .collect()
    }

    pub async fn update_account_auth(
        &self,
        account_id: &str,
        auth_file: &CodexAuthFile,
        status: AccountStatus,
        refreshed_at: Option<u64>,
    ) -> Result<(), DataError> {
        let now = unix_now_secs() as i64;
        sqlx::query(
            r#"
UPDATE accounts
SET auth_file_ciphertext = ?, access_token_preview = ?, refresh_token_preview = ?,
    expires_at = ?, last_refresh_at = COALESCE(?, last_refresh_at),
    status = CASE WHEN redeemed_at IS NULL THEN ? ELSE status END,
    updated_at = ?
WHERE id = ? AND redeemed_at IS NULL
"#,
        )
        .bind(self.secrets.encrypt_json(&auth_file.clone().normalized())?)
        .bind(secret_preview(auth_file.access_token.as_deref()))
        .bind(secret_preview(auth_file.refresh_token.as_deref()))
        .bind(auth_file.expires_at_epoch().map(|value| value as i64))
        .bind(refreshed_at.map(|value| value as i64))
        .bind(status.as_str())
        .bind(now)
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_account_status(
        &self,
        account_id: &str,
        status: AccountStatus,
    ) -> Result<(), DataError> {
        sqlx::query("UPDATE accounts SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(unix_now_secs() as i64)
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn record_health_check(
        &self,
        account_id: &str,
        result: &HealthCheckResult,
        http_status: Option<u16>,
        latency_ms: Option<u64>,
    ) -> Result<(), DataError> {
        let now = unix_now_secs() as i64;
        sqlx::query(
            r#"
INSERT INTO account_health_checks (
  id, account_id, status, http_status, latency_ms, quota_snapshot, error, created_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(account_id)
        .bind(result.status.as_str())
        .bind(http_status.map(|value| value as i64))
        .bind(latency_ms.map(|value| value as i64))
        .bind(result.quota_snapshot.as_ref().map(Value::to_string))
        .bind(&result.error)
        .bind(now)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            r#"
UPDATE accounts
SET status = ?,
    plan_type = COALESCE(?, plan_type),
    quota_snapshot = ?,
    last_probe_at = ?,
    updated_at = ?
WHERE id = ?
"#,
        )
        .bind(result.status.as_str())
        .bind(&result.plan_type)
        .bind(result.quota_snapshot.as_ref().map(Value::to_string))
        .bind(now)
        .bind(now)
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_auto_probe_settings(&self) -> Result<AutoProbeSettings, DataError> {
        let row = sqlx::query("SELECT value_json, updated_at FROM app_settings WHERE key = ?")
            .bind(AUTO_PROBE_SETTINGS_KEY)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(AutoProbeSettings::default());
        };
        let value_json: String = row.try_get("value_json")?;
        let updated_at = optional_i64(&row, "updated_at")?.unwrap_or_default();
        let mut settings = serde_json::from_str::<AutoProbeSettings>(&value_json)
            .unwrap_or_else(|_| AutoProbeSettings::default());
        settings.updated_at = updated_at;
        Ok(settings.normalized())
    }

    pub async fn save_auto_probe_settings(
        &self,
        settings: &AutoProbeSettings,
    ) -> Result<AutoProbeSettings, DataError> {
        let mut settings = settings.clone().normalized();
        settings.updated_at = unix_now_secs();
        sqlx::query(
            r#"
INSERT INTO app_settings (key, value_json, updated_at)
VALUES (?, ?, ?)
ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at
"#,
        )
        .bind(AUTO_PROBE_SETTINGS_KEY)
        .bind(serde_json::to_string(&settings).map_err(|_| DataError::Encryption)?)
        .bind(settings.updated_at as i64)
        .execute(&self.pool)
        .await?;
        Ok(settings)
    }

    pub async fn mark_auto_probe_started(&self, started_at: u64) -> Result<(), DataError> {
        let mut settings = self.get_auto_probe_settings().await?;
        settings.last_started_at = Some(started_at);
        settings.last_error = None;
        self.save_auto_probe_settings(&settings).await?;
        Ok(())
    }

    pub async fn mark_auto_probe_finished(
        &self,
        finished_at: u64,
        checked_count: u64,
        result: Value,
        error: Option<String>,
    ) -> Result<AutoProbeSettings, DataError> {
        let mut settings = self.get_auto_probe_settings().await?;
        settings.last_finished_at = Some(finished_at);
        settings.last_checked_count = checked_count;
        settings.last_result = Some(result);
        settings.last_error = error;
        self.save_auto_probe_settings(&settings).await
    }

    pub async fn create_redeem_batch(
        &self,
        input: CreateRedeemBatchInput,
    ) -> Result<CreateRedeemBatchOutcome, DataError> {
        let now = unix_now_secs() as i64;
        let batch_id = Uuid::new_v4().to_string();
        let plan_filter_json = input
            .plan_filter
            .as_ref()
            .map(|value| json!(value).to_string());
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
INSERT INTO redeem_code_batches (
  id, name, status, total_count, redeemed_count, accounts_per_code,
  plan_filter_json, expires_at, created_at, updated_at
) VALUES (?, ?, 'active', ?, 0, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&batch_id)
        .bind(input.name.trim())
        .bind(input.total_count as i64)
        .bind(input.accounts_per_code as i64)
        .bind(plan_filter_json)
        .bind(input.expires_at.map(|value| value as i64))
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        let mut codes = Vec::new();
        while codes.len() < input.total_count {
            let formatted = generate_redeem_code();
            let Some(normalized) = normalize_redeem_code(&formatted) else {
                continue;
            };
            let hash = redeem_code_hash(&normalized);
            let code = format_redeem_code(&normalized);
            let code_ciphertext = self.secrets.encrypt_json(&code)?;
            let code_id = Uuid::new_v4().to_string();
            let prefix = normalized.chars().take(4).collect::<String>();
            let suffix = normalized
                .chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            let masked_code = mask_redeem_code(&normalized);
            let inserted = sqlx::query(
                r#"
INSERT OR IGNORE INTO redeem_codes (
  id, batch_id, code_hash, code_prefix, code_suffix, masked_code, code_ciphertext,
  status, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, 'active', ?, ?)
"#,
            )
            .bind(&code_id)
            .bind(&batch_id)
            .bind(hash)
            .bind(prefix)
            .bind(suffix)
            .bind(&masked_code)
            .bind(code_ciphertext)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            if inserted.rows_affected() == 1 {
                codes.push(RedeemCodeCreated {
                    id: code_id,
                    code,
                    masked_code,
                });
            }
        }
        tx.commit().await?;
        Ok(CreateRedeemBatchOutcome { batch_id, codes })
    }

    pub async fn list_redeem_batches(&self) -> Result<Vec<RedeemBatchSummary>, DataError> {
        let rows = sqlx::query(
            r#"
SELECT id, name, status, total_count, redeemed_count, accounts_per_code,
       plan_filter_json, expires_at, created_at, updated_at
FROM redeem_code_batches
ORDER BY created_at DESC
"#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(batch_summary_from_row).collect()
    }

    pub async fn list_redeem_codes(
        &self,
        batch_id: &str,
    ) -> Result<Vec<RedeemCodeSummary>, DataError> {
        let rows = sqlx::query(
            r#"
SELECT id, batch_id, masked_code, code_ciphertext, status, redemption_id, redeemed_at, created_at, updated_at
FROM redeem_codes
WHERE batch_id = ?
ORDER BY created_at ASC
"#,
        )
        .bind(batch_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| code_summary_from_row(row, &self.secrets))
            .collect()
    }

    pub async fn prepare_redeem_export(
        &self,
        raw_codes: &[String],
    ) -> Result<RedeemExportPreparation, DataError> {
        let now = unix_now_secs();
        let mut demands = Vec::new();
        let mut seen_hashes = HashSet::new();
        let mut estimated_account_count = 0_usize;
        for raw_code in raw_codes {
            let Some(normalized) = normalize_redeem_code(raw_code) else {
                continue;
            };
            let hash = redeem_code_hash(&normalized);
            if !seen_hashes.insert(hash.clone()) {
                continue;
            }
            let Some(row) = sqlx::query(
                r#"
SELECT codes.status AS code_status, codes.redemption_id,
       batches.status AS batch_status, batches.accounts_per_code,
       batches.plan_filter_json, batches.expires_at
FROM redeem_codes AS codes
JOIN redeem_code_batches AS batches ON batches.id = codes.batch_id
WHERE codes.code_hash = ?
"#,
            )
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?
            else {
                continue;
            };

            let code_status: String = row.try_get("code_status")?;
            let batch_status: String = row.try_get("batch_status")?;
            let redemption_id: Option<String> = row.try_get("redemption_id")?;
            let expires_at: Option<i64> = row.try_get("expires_at")?;
            if code_status != "active"
                || batch_status != "active"
                || expires_at.is_some_and(|value| value <= now as i64)
            {
                continue;
            }
            if let Some(redemption_id) = redemption_id {
                if let Some(row) =
                    sqlx::query("SELECT account_ids_json FROM redeem_redemptions WHERE id = ?")
                        .bind(redemption_id)
                        .fetch_optional(&self.pool)
                        .await?
                {
                    let account_ids = serde_json::from_str::<Vec<String>>(
                        row.try_get::<String, _>("account_ids_json")?.as_str(),
                    )
                    .unwrap_or_default();
                    estimated_account_count =
                        estimated_account_count.saturating_add(account_ids.len());
                }
                continue;
            }
            let plan_filter = row
                .try_get::<Option<String>, _>("plan_filter_json")?
                .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                .unwrap_or_default();
            let accounts_per_code: i64 = row.try_get("accounts_per_code")?;
            if accounts_per_code > 0 {
                estimated_account_count =
                    estimated_account_count.saturating_add(accounts_per_code as usize);
                demands.push(RedeemAccountDemand {
                    count: accounts_per_code as usize,
                    plan_filter,
                });
            }
        }
        if demands.is_empty() {
            return Ok(RedeemExportPreparation {
                estimated_account_count,
                refresh_account_ids: Vec::new(),
            });
        }

        let rows = sqlx::query(
            r#"
SELECT id, plan_type, status, expires_at
FROM accounts
WHERE redeemed_at IS NULL AND status IN ('available', 'at_expired')
ORDER BY created_at ASC
"#,
        )
        .fetch_all(&self.pool)
        .await?;
        let candidates = rows
            .into_iter()
            .map(|row| {
                Ok(RedeemCandidateAccount {
                    id: row.try_get("id")?,
                    plan_type: row.try_get("plan_type")?,
                    status: row.try_get("status")?,
                    expires_at: optional_i64(&row, "expires_at")?,
                })
            })
            .collect::<Result<Vec<_>, DataError>>()?;

        let mut selected_ids = HashSet::new();
        let mut refresh_ids = Vec::new();
        for demand in demands {
            let mut remaining = demand.count;
            for candidate in &candidates {
                if remaining == 0 {
                    break;
                }
                if selected_ids.contains(&candidate.id) || !candidate.matches(&demand.plan_filter) {
                    continue;
                }
                if candidate.is_usable(now) {
                    selected_ids.insert(candidate.id.clone());
                    remaining -= 1;
                }
            }
            for candidate in &candidates {
                if remaining == 0 {
                    break;
                }
                if selected_ids.contains(&candidate.id) || !candidate.matches(&demand.plan_filter) {
                    continue;
                }
                if candidate.needs_refresh(now) {
                    selected_ids.insert(candidate.id.clone());
                    refresh_ids.push(candidate.id.clone());
                    remaining -= 1;
                }
            }
        }
        Ok(RedeemExportPreparation {
            estimated_account_count,
            refresh_account_ids: refresh_ids,
        })
    }

    pub async fn redeem_codes_for_export(
        &self,
        raw_codes: &[String],
        format: ExportFormat,
    ) -> Result<RedeemExportOutcome, DataError> {
        let _redeem_guard = self.redemption_lock.lock().await;
        let mut successes = Vec::new();
        let mut failures = Vec::new();
        let mut all_auth_files = Vec::new();
        let mut all_account_ids = Vec::new();
        let now = unix_now_secs() as i64;
        let usable_after = now.saturating_add(ACCESS_TOKEN_REFRESH_GRACE_SECONDS as i64);
        let mut seen_hashes = HashSet::new();
        let mut tx = self.pool.begin().await?;

        for raw_code in raw_codes {
            let Some(normalized) = normalize_redeem_code(raw_code) else {
                failures.push(RedeemFailure {
                    code: raw_code.clone(),
                    reason: "兑换码格式无效".to_string(),
                });
                continue;
            };
            let hash = redeem_code_hash(&normalized);
            if !seen_hashes.insert(hash.clone()) {
                failures.push(RedeemFailure {
                    code: format_redeem_code(&normalized),
                    reason: "兑换码重复提交".to_string(),
                });
                continue;
            }
            let Some(code_row) = sqlx::query(
                r#"
SELECT codes.id AS code_id, codes.batch_id, codes.status AS code_status,
       codes.redemption_id, batches.status AS batch_status,
       batches.accounts_per_code, batches.plan_filter_json, batches.expires_at
FROM redeem_codes AS codes
JOIN redeem_code_batches AS batches ON batches.id = codes.batch_id
WHERE codes.code_hash = ?
"#,
            )
            .bind(hash)
            .fetch_optional(&mut *tx)
            .await?
            else {
                failures.push(RedeemFailure {
                    code: format_redeem_code(&normalized),
                    reason: "兑换码不存在".to_string(),
                });
                continue;
            };

            let code_id: String = code_row.try_get("code_id")?;
            let batch_id: String = code_row.try_get("batch_id")?;
            let code_status: String = code_row.try_get("code_status")?;
            let batch_status: String = code_row.try_get("batch_status")?;
            let accounts_per_code: i64 = code_row.try_get("accounts_per_code")?;
            let expires_at: Option<i64> = code_row.try_get("expires_at")?;
            let plan_filter: Option<String> = code_row.try_get("plan_filter_json")?;

            if batch_status != "active" || code_status == "disabled" {
                failures.push(RedeemFailure {
                    code: format_redeem_code(&normalized),
                    reason: "兑换码已停用".to_string(),
                });
                continue;
            }
            if expires_at.is_some_and(|value| value <= now) {
                failures.push(RedeemFailure {
                    code: format_redeem_code(&normalized),
                    reason: "兑换码已过期".to_string(),
                });
                continue;
            }

            let redemption_id: Option<String> = code_row.try_get("redemption_id")?;
            let (account_ids, auth_files) = if let Some(redemption_id) = redemption_id {
                let row = sqlx::query(
                    "SELECT account_ids_json, export_snapshot_ciphertext FROM redeem_redemptions WHERE id = ?",
                )
                .bind(&redemption_id)
                .fetch_one(&mut *tx)
                .await?;
                let snapshot_ciphertext: String = row.try_get("export_snapshot_ciphertext")?;
                let auth_files = self
                    .secrets
                    .decrypt_json::<Vec<CodexAuthFile>>(&snapshot_ciphertext)?;
                let account_ids = serde_json::from_str::<Vec<String>>(
                    row.try_get::<String, _>("account_ids_json")?.as_str(),
                )
                .unwrap_or_default();
                (account_ids, auth_files)
            } else {
                let plan_filter = plan_filter
                    .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                    .unwrap_or_default();
                let rows = sqlx::query(
                    r#"
SELECT id, plan_type
FROM accounts
WHERE redeemed_at IS NULL AND status = 'available' AND expires_at IS NOT NULL AND expires_at > ?
ORDER BY created_at ASC
"#,
                )
                .bind(usable_after)
                .fetch_all(&mut *tx)
                .await?;
                let account_ids = rows
                    .into_iter()
                    .filter_map(|row| {
                        let id: String = row.try_get("id").ok()?;
                        let plan_type: Option<String> = row.try_get("plan_type").ok();
                        if !plan_filter.is_empty()
                            && !plan_type.as_ref().is_some_and(|value| {
                                plan_filter.iter().any(|p| p.eq_ignore_ascii_case(value))
                            })
                        {
                            return None;
                        }
                        Some(id)
                    })
                    .take(accounts_per_code as usize)
                    .collect::<Vec<_>>();
                if account_ids.len() < accounts_per_code as usize {
                    failures.push(RedeemFailure {
                        code: format_redeem_code(&normalized),
                        reason: "可兑换账号库存不足".to_string(),
                    });
                    continue;
                }
                let redemption_id = Uuid::new_v4().to_string();
                let auth_files = self
                    .load_auth_files_for_ids_tx(&mut tx, &account_ids)
                    .await?
                    .into_iter()
                    .map(|(_, auth)| auth.normalized())
                    .collect::<Vec<_>>();
                let auth_files_snapshot = self.secrets.encrypt_json(&auth_files)?;
                sqlx::query(
                    r#"
INSERT INTO redeem_redemptions (
  id, code_id, batch_id, export_format, account_ids_json, export_snapshot_ciphertext, created_at
) VALUES (?, ?, ?, ?, ?, ?, ?)
"#,
                )
                .bind(&redemption_id)
                .bind(&code_id)
                .bind(&batch_id)
                .bind(format.as_str())
                .bind(serde_json::to_string(&account_ids).unwrap_or_else(|_| "[]".to_string()))
                .bind(auth_files_snapshot)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                for account_id in &account_ids {
                    let updated = sqlx::query(
                        r#"
UPDATE accounts
SET redeemed_at = ?, redeem_code_id = ?, redemption_id = ?, updated_at = ?
WHERE id = ? AND redeemed_at IS NULL
"#,
                    )
                    .bind(now)
                    .bind(&code_id)
                    .bind(&redemption_id)
                    .bind(now)
                    .bind(account_id)
                    .execute(&mut *tx)
                    .await?;
                    if updated.rows_affected() != 1 {
                        return Err(DataError::NotFound);
                    }
                }
                sqlx::query(
                    "UPDATE redeem_codes SET status = 'redeemed', redemption_id = ?, redeemed_at = ?, updated_at = ? WHERE id = ?",
                )
                .bind(&redemption_id)
                .bind(now)
                .bind(now)
                .bind(&code_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE redeem_code_batches SET redeemed_count = redeemed_count + 1, updated_at = ? WHERE id = ?",
                )
                .bind(now)
                .bind(&batch_id)
                .execute(&mut *tx)
                .await?;
                (account_ids, auth_files)
            };

            successes.push(RedeemSuccess {
                code: format_redeem_code(&normalized),
                account_count: account_ids.len(),
            });
            all_account_ids.extend(account_ids);
            all_auth_files.extend(auth_files);
        }

        let document = export_accounts(format, &all_auth_files);
        let export_id = Uuid::new_v4().to_string();
        let account_ids_json = json!(all_account_ids).to_string();
        sqlx::query(
            "INSERT INTO account_exports (id, format, source, account_ids_json, account_count, created_at) VALUES (?, ?, 'redeem', ?, ?, ?)",
        )
        .bind(export_id)
        .bind(format.as_str())
        .bind(account_ids_json)
        .bind(all_auth_files.len() as i64)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(RedeemExportOutcome {
            format,
            document,
            successes,
            failures,
        })
    }

    pub async fn export_admin_accounts(
        &self,
        ids: Option<&[String]>,
        include_redeemed: bool,
        format: ExportFormat,
    ) -> Result<Value, DataError> {
        let accounts = if let Some(ids) = ids {
            self.load_auth_files_for_ids(ids, include_redeemed).await?
        } else {
            self.load_all_auth_files()
                .await?
                .into_iter()
                .filter(|(summary, _)| include_redeemed || summary.redeemed_at.is_none())
                .collect()
        };
        let auth_files = accounts
            .into_iter()
            .map(|(_, auth)| auth)
            .collect::<Vec<_>>();
        Ok(export_accounts(format, &auth_files))
    }

    async fn load_all_auth_files(&self) -> Result<Vec<(AccountSummary, CodexAuthFile)>, DataError> {
        let rows = sqlx::query(
            r#"
SELECT a.id, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at, a.auth_file_ciphertext
FROM accounts a
LEFT JOIN redeem_codes rc ON rc.id = a.redeem_code_id
ORDER BY a.created_at ASC
"#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| self.auth_pair_from_row(row))
            .collect()
    }

    fn auth_pair_from_row(
        &self,
        row: sqlx::sqlite::SqliteRow,
    ) -> Result<(AccountSummary, CodexAuthFile), DataError> {
        let summary = account_summary_from_row(&row)?;
        let ciphertext: String = row.try_get("auth_file_ciphertext")?;
        let auth_file = self.secrets.decrypt_json::<CodexAuthFile>(&ciphertext)?;
        Ok((summary, auth_file.normalized()))
    }

    async fn load_auth_files_for_ids_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        ids: &[String],
    ) -> Result<Vec<(AccountSummary, CodexAuthFile)>, DataError> {
        let mut out = Vec::new();
        for id in ids {
            let row = sqlx::query(
                r#"
SELECT a.id, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at, a.auth_file_ciphertext
FROM accounts a
LEFT JOIN redeem_codes rc ON rc.id = a.redeem_code_id
WHERE a.id = ?
"#,
            )
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
            let summary = account_summary_from_row(&row)?;
            let ciphertext: String = row.try_get("auth_file_ciphertext")?;
            out.push((
                summary,
                self.secrets.decrypt_json::<CodexAuthFile>(&ciphertext)?,
            ));
        }
        Ok(out)
    }
}

async fn ensure_schema_upgrades(pool: &SqlitePool) -> Result<(), DataError> {
    ensure_sqlite_column(
        pool,
        "redeem_codes",
        "code_ciphertext",
        "ALTER TABLE redeem_codes ADD COLUMN code_ciphertext TEXT",
    )
    .await
}

async fn ensure_sqlite_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    alter_sql: &str,
) -> Result<(), DataError> {
    let pragma = format!("PRAGMA table_info({table})");
    let rows = sqlx::query(&pragma).fetch_all(pool).await?;
    let exists = rows.iter().any(|row| {
        row.try_get::<String, _>("name")
            .is_ok_and(|name| name == column)
    });
    if !exists {
        sqlx::query(alter_sql).execute(pool).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportAccountsOutcome {
    pub imported: usize,
    pub updated: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountListQuery {
    pub search: Option<String>,
    pub status: Option<String>,
    pub redeemed: Option<bool>,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountListPage {
    pub items: Vec<AccountSummary>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeleteAccountsOutcome {
    pub deleted: usize,
    pub skipped: usize,
    pub not_found: usize,
    pub results: Vec<DeleteAccountResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeleteAccountResult {
    pub account_id: String,
    pub status: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountSummary {
    pub id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub account_id: Option<String>,
    pub plan_type: Option<String>,
    pub status: String,
    pub access_token_preview: Option<String>,
    pub refresh_token_preview: Option<String>,
    pub expires_at: Option<u64>,
    pub last_refresh_at: Option<u64>,
    pub last_probe_at: Option<u64>,
    pub redeem_code_id: Option<String>,
    pub redeem_code_masked: Option<String>,
    pub redemption_id: Option<String>,
    pub redeemed_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoProbeSettings {
    pub enabled: bool,
    pub interval_seconds: u64,
    pub max_accounts_per_run: u64,
    pub concurrency: u64,
    pub refresh_before_probe: bool,
    #[serde(default)]
    pub proxy_enabled: bool,
    #[serde(default = "default_probe_proxy_mode")]
    pub proxy_mode: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub proxy_api_url: Option<String>,
    #[serde(default = "default_probe_proxy_scheme")]
    pub proxy_default_scheme: String,
    pub last_started_at: Option<u64>,
    pub last_finished_at: Option<u64>,
    pub last_checked_count: u64,
    pub last_error: Option<String>,
    pub last_result: Option<Value>,
    pub updated_at: u64,
}

impl Default for AutoProbeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 60 * 60,
            max_accounts_per_run: 100,
            concurrency: 4,
            refresh_before_probe: false,
            proxy_enabled: false,
            proxy_mode: default_probe_proxy_mode(),
            proxy_url: None,
            proxy_api_url: None,
            proxy_default_scheme: default_probe_proxy_scheme(),
            last_started_at: None,
            last_finished_at: None,
            last_checked_count: 0,
            last_error: None,
            last_result: None,
            updated_at: 0,
        }
    }
}

impl AutoProbeSettings {
    pub fn normalized(mut self) -> Self {
        self.interval_seconds = self.interval_seconds.clamp(60, 24 * 60 * 60);
        self.max_accounts_per_run = self.max_accounts_per_run.clamp(1, 5_000);
        self.concurrency = self.concurrency.clamp(1, 32);
        self.refresh_before_probe = false;
        self.proxy_mode = match self.proxy_mode.trim().to_ascii_lowercase().as_str() {
            "api" | "dynamic_api" | "711" => "api".to_string(),
            _ => "fixed".to_string(),
        };
        self.proxy_default_scheme = match self
            .proxy_default_scheme
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "socks" | "socks5" => "socks5".to_string(),
            "socks5h" => "socks5h".to_string(),
            _ => "http".to_string(),
        };
        self.proxy_url = self
            .proxy_url
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.proxy_api_url = self
            .proxy_api_url
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self
    }
}

fn default_probe_proxy_mode() -> String {
    "fixed".to_string()
}

fn default_probe_proxy_scheme() -> String {
    "http".to_string()
}

struct RedeemAccountDemand {
    count: usize,
    plan_filter: Vec<String>,
}

struct RedeemCandidateAccount {
    id: String,
    plan_type: Option<String>,
    status: String,
    expires_at: Option<u64>,
}

impl RedeemCandidateAccount {
    fn matches(&self, plan_filter: &[String]) -> bool {
        plan_filter.is_empty()
            || self.plan_type.as_ref().is_some_and(|value| {
                plan_filter
                    .iter()
                    .any(|plan| plan.eq_ignore_ascii_case(value))
            })
    }

    fn is_usable(&self, now: u64) -> bool {
        self.status == AccountStatus::Available.as_str()
            && self.expires_at.is_some_and(|expires_at| {
                !access_token_needs_refresh(
                    Some(expires_at),
                    now,
                    ACCESS_TOKEN_REFRESH_GRACE_SECONDS,
                )
            })
    }

    fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at.is_none_or(|expires_at| {
            access_token_needs_refresh(Some(expires_at), now, ACCESS_TOKEN_REFRESH_GRACE_SECONDS)
        })
    }
}

fn account_summary_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<AccountSummary, DataError> {
    Ok(AccountSummary {
        id: row.try_get("id")?,
        email: row.try_get("email")?,
        name: row.try_get("name")?,
        account_id: row.try_get("account_id")?,
        plan_type: row.try_get("plan_type")?,
        status: row.try_get("status")?,
        access_token_preview: row.try_get("access_token_preview")?,
        refresh_token_preview: row.try_get("refresh_token_preview")?,
        expires_at: optional_i64(row, "expires_at")?,
        last_refresh_at: optional_i64(row, "last_refresh_at")?,
        last_probe_at: optional_i64(row, "last_probe_at")?,
        redeem_code_id: row.try_get("redeem_code_id")?,
        redeem_code_masked: row.try_get("redeem_code_masked")?,
        redemption_id: row.try_get("redemption_id")?,
        redeemed_at: optional_i64(row, "redeemed_at")?,
        created_at: optional_i64(row, "created_at")?.unwrap_or_default(),
        updated_at: optional_i64(row, "updated_at")?.unwrap_or_default(),
    })
}

fn optional_i64(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<Option<u64>, DataError> {
    Ok(row
        .try_get::<Option<i64>, _>(name)?
        .and_then(|value| u64::try_from(value).ok()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRedeemBatchInput {
    pub name: String,
    pub total_count: usize,
    pub accounts_per_code: usize,
    pub expires_at: Option<u64>,
    pub plan_filter: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateRedeemBatchOutcome {
    pub batch_id: String,
    pub codes: Vec<RedeemCodeCreated>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemCodeCreated {
    pub id: String,
    pub code: String,
    pub masked_code: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemBatchSummary {
    pub id: String,
    pub name: String,
    pub status: String,
    pub total_count: u64,
    pub redeemed_count: u64,
    pub accounts_per_code: u64,
    pub plan_filter: Vec<String>,
    pub expires_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

fn batch_summary_from_row(row: sqlx::sqlite::SqliteRow) -> Result<RedeemBatchSummary, DataError> {
    let plan_filter = row
        .try_get::<Option<String>, _>("plan_filter_json")?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    Ok(RedeemBatchSummary {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        status: row.try_get("status")?,
        total_count: optional_i64(&row, "total_count")?.unwrap_or_default(),
        redeemed_count: optional_i64(&row, "redeemed_count")?.unwrap_or_default(),
        accounts_per_code: optional_i64(&row, "accounts_per_code")?.unwrap_or_default(),
        plan_filter,
        expires_at: optional_i64(&row, "expires_at")?,
        created_at: optional_i64(&row, "created_at")?.unwrap_or_default(),
        updated_at: optional_i64(&row, "updated_at")?.unwrap_or_default(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemCodeSummary {
    pub id: String,
    pub batch_id: String,
    pub code: Option<String>,
    pub masked_code: String,
    pub status: String,
    pub redemption_id: Option<String>,
    pub redeemed_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

fn code_summary_from_row(
    row: sqlx::sqlite::SqliteRow,
    secrets: &SecretBox,
) -> Result<RedeemCodeSummary, DataError> {
    let code = row
        .try_get::<Option<String>, _>("code_ciphertext")?
        .as_deref()
        .and_then(|ciphertext| secrets.decrypt_json::<String>(ciphertext).ok());
    Ok(RedeemCodeSummary {
        id: row.try_get("id")?,
        batch_id: row.try_get("batch_id")?,
        code,
        masked_code: row.try_get("masked_code")?,
        status: row.try_get("status")?,
        redemption_id: row.try_get("redemption_id")?,
        redeemed_at: optional_i64(&row, "redeemed_at")?,
        created_at: optional_i64(&row, "created_at")?.unwrap_or_default(),
        updated_at: optional_i64(&row, "updated_at")?.unwrap_or_default(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemExportOutcome {
    pub format: ExportFormat,
    pub document: Value,
    pub successes: Vec<RedeemSuccess>,
    pub failures: Vec<RedeemFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemExportPreparation {
    pub estimated_account_count: usize,
    pub refresh_account_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemSuccess {
    pub code: String,
    pub account_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemFailure {
    pub code: String,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_repo() -> AccountPoolRepository {
        let path =
            std::env::temp_dir().join(format!("aether-pool-test-{}.sqlite3", Uuid::new_v4()));
        AccountPoolRepository::connect(&format!("sqlite://{}", path.display()), "test-secret")
            .await
            .unwrap()
    }

    fn parsed_account(account_id: &str, access_token: &str) -> ParsedAccount {
        ParsedAccount {
            source: "test".to_string(),
            auth_file: CodexAuthFile {
                kind: Some("codex".to_string()),
                account_id: Some(account_id.to_string()),
                chatgpt_account_id: Some(account_id.to_string()),
                email: Some(format!("{account_id}@example.com")),
                name: Some(account_id.to_string()),
                plan_type: Some("plus".to_string()),
                chatgpt_plan_type: Some("plus".to_string()),
                access_token: Some(access_token.to_string()),
                refresh_token: Some(format!("refresh-{account_id}")),
                expires_at: Some(json!(2_000_000_000_u64)),
                ..CodexAuthFile::default()
            },
        }
    }

    fn parsed_expired_account(account_id: &str, access_token: &str) -> ParsedAccount {
        ParsedAccount {
            source: "test".to_string(),
            auth_file: CodexAuthFile {
                kind: Some("codex".to_string()),
                account_id: Some(account_id.to_string()),
                chatgpt_account_id: Some(account_id.to_string()),
                email: Some(format!("{account_id}@example.com")),
                name: Some(account_id.to_string()),
                plan_type: Some("plus".to_string()),
                chatgpt_plan_type: Some("plus".to_string()),
                access_token: Some(access_token.to_string()),
                refresh_token: Some(format!("refresh-{account_id}")),
                expires_at: Some(json!(1_u64)),
                ..CodexAuthFile::default()
            },
        }
    }

    fn parsed_workspace_account(email: &str, access_token: &str) -> ParsedAccount {
        ParsedAccount {
            source: "test".to_string(),
            auth_file: CodexAuthFile {
                kind: Some("codex".to_string()),
                account_id: Some("shared-workspace".to_string()),
                chatgpt_account_id: Some("shared-workspace".to_string()),
                email: Some(email.to_string()),
                name: Some(email.to_string()),
                plan_type: Some("plus".to_string()),
                chatgpt_plan_type: Some("plus".to_string()),
                access_token: Some(access_token.to_string()),
                refresh_token: Some(format!("refresh-{email}")),
                expires_at: Some(json!(2_000_000_000_u64)),
                ..CodexAuthFile::default()
            },
        }
    }

    fn document_access_token(value: &Value) -> String {
        value
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn imports_same_workspace_accounts_as_distinct_emails() {
        let repo = temp_repo().await;
        let outcome = repo
            .import_accounts(&[
                parsed_workspace_account("alpha@example.com", "access-alpha"),
                parsed_workspace_account("beta@example.com", "access-beta"),
            ])
            .await
            .unwrap();
        assert_eq!(outcome.imported, 2);
        assert_eq!(outcome.updated, 0);

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(
            page.items
                .iter()
                .filter(|account| account.account_id.as_deref() == Some("shared-workspace"))
                .count(),
            2
        );

        let update = repo
            .import_accounts(&[parsed_workspace_account(
                "ALPHA@example.com",
                "access-alpha-updated",
            )])
            .await
            .unwrap();
        assert_eq!(update.imported, 0);
        assert_eq!(update.updated, 1);

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 2);
    }

    #[tokio::test]
    async fn import_migrates_matching_legacy_workspace_fingerprint() {
        let repo = temp_repo().await;
        let first = parsed_workspace_account("legacy@example.com", "access-legacy");
        repo.import_accounts(std::slice::from_ref(&first))
            .await
            .unwrap();
        let auth_file = first.auth_file.normalized();
        let legacy_fingerprint = legacy_fingerprint_auth_file(&auth_file);
        sqlx::query("UPDATE accounts SET auth_fingerprint = ?")
            .bind(legacy_fingerprint)
            .execute(repo.pool())
            .await
            .unwrap();

        let update = repo
            .import_accounts(&[parsed_workspace_account(
                "legacy@example.com",
                "access-legacy-updated",
            )])
            .await
            .unwrap();
        assert_eq!(update.imported, 0);
        assert_eq!(update.updated, 1);

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
    }

    #[tokio::test]
    async fn deletes_only_unbound_accounts_and_cascades_health_checks() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("acct-1", "access-1")])
            .await
            .unwrap();
        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let account_id = page.items[0].id.clone();
        repo.record_health_check(
            &account_id,
            &HealthCheckResult {
                status: AccountStatus::Available,
                plan_type: Some("plus".to_string()),
                quota_snapshot: Some(json!({ "primary_used_percent": 5.0 })),
                error: None,
            },
            Some(200),
            Some(42),
        )
        .await
        .unwrap();

        let outcome = repo
            .delete_unbound_accounts(&[account_id.clone(), "missing-account".to_string()])
            .await
            .unwrap();
        assert_eq!(outcome.deleted, 1);
        assert_eq!(outcome.not_found, 1);
        assert_eq!(outcome.results[0].status, "deleted");

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 0);
        let row = sqlx::query("SELECT COUNT(*) AS count FROM account_health_checks")
            .fetch_one(repo.pool())
            .await
            .unwrap();
        let health_count: i64 = row.try_get("count").unwrap();
        assert_eq!(health_count, 0);
    }

    #[tokio::test]
    async fn delete_skips_redeemed_accounts() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("acct-1", "access-1"),
            parsed_account("acct-2", "access-2"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "delete guard".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        repo.redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let redeemed = page
            .items
            .iter()
            .find(|account| account.redeemed_at.is_some())
            .unwrap()
            .id
            .clone();
        let available = page
            .items
            .iter()
            .find(|account| account.redeemed_at.is_none())
            .unwrap()
            .id
            .clone();

        let outcome = repo
            .delete_unbound_accounts(&[redeemed.clone(), available])
            .await
            .unwrap();
        assert_eq!(outcome.deleted, 1);
        assert_eq!(outcome.skipped, 1);

        let remaining = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(remaining.total, 1);
        assert_eq!(remaining.items[0].id, redeemed);
        assert!(remaining.items[0].redeem_code_id.is_some());
    }

    #[tokio::test]
    async fn list_redeem_codes_returns_full_codes_for_new_batches() {
        let repo = temp_repo().await;
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "full code export".to_string(),
                total_count: 3,
                accounts_per_code: 1,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let codes = repo.list_redeem_codes(&batch.batch_id).await.unwrap();
        assert_eq!(codes.len(), 3);
        assert_eq!(
            codes
                .iter()
                .map(|code| code.code.as_deref())
                .collect::<Vec<_>>(),
            batch
                .codes
                .iter()
                .map(|code| Some(code.code.as_str()))
                .collect::<Vec<_>>()
        );
        assert!(codes
            .iter()
            .zip(batch.codes.iter())
            .all(|(listed, created)| listed.masked_code == created.masked_code));
    }

    #[tokio::test]
    async fn redeemed_accounts_are_retained_and_skipped_by_unredeemed_loads() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("acct-1", "access-1"),
            parsed_account("acct-2", "access-2"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "test".to_string(),
                total_count: 2,
                accounts_per_code: 1,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let first = repo
            .redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        assert_eq!(first.successes.len(), 1);
        assert_eq!(first.successes[0].account_count, 1);
        let first_snapshot_token = document_access_token(&first.document);

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(
            page.items
                .iter()
                .filter(|account| account.redeemed_at.is_some())
                .count(),
            1
        );
        let redeemed_account = page
            .items
            .iter()
            .find(|account| account.redeemed_at.is_some())
            .unwrap();
        assert_eq!(redeemed_account.status, AccountStatus::Available.as_str());
        assert_eq!(
            redeemed_account.redeem_code_masked.as_deref(),
            Some(batch.codes[0].masked_code.as_str())
        );
        let available_account = page
            .items
            .iter()
            .find(|account| account.redeemed_at.is_none())
            .unwrap();
        assert_eq!(
            repo.load_unredeemed_auth_files(None).await.unwrap().len(),
            1
        );
        repo.record_health_check(
            &redeemed_account.id,
            &HealthCheckResult {
                status: AccountStatus::QuotaExhausted,
                plan_type: Some("plus".to_string()),
                quota_snapshot: Some(json!({ "primary_used_percent": 100.0 })),
                error: None,
            },
            Some(200),
            Some(50),
        )
        .await
        .unwrap();
        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let measured_redeemed_account = page
            .items
            .iter()
            .find(|account| account.id == redeemed_account.id)
            .unwrap();
        assert_eq!(
            measured_redeemed_account.status,
            AccountStatus::QuotaExhausted.as_str()
        );
        assert!(measured_redeemed_account.redeem_code_id.is_some());
        sqlx::query("UPDATE accounts SET auth_file_ciphertext = 'bad-ciphertext' WHERE id = ?")
            .bind(&redeemed_account.id)
            .execute(repo.pool())
            .await
            .unwrap();
        assert_eq!(
            repo.load_auth_files_for_ids(std::slice::from_ref(&redeemed_account.id), false)
                .await
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            repo.load_auth_files_for_ids(std::slice::from_ref(&available_account.id), false)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            repo.load_unredeemed_auth_files(None).await.unwrap().len(),
            1
        );

        repo.import_accounts(&[parsed_account("acct-1", "access-1-refreshed")])
            .await
            .unwrap();
        let repeat = repo
            .redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        assert_eq!(
            document_access_token(&repeat.document),
            first_snapshot_token
        );

        let second = repo
            .redeem_codes_for_export(&[batch.codes[1].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        assert_eq!(second.successes.len(), 1);
        assert_eq!(
            repo.load_unredeemed_auth_files(None).await.unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn redeem_does_not_export_expired_accounts_without_refresh() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_expired_account("expired-1", "expired-access")])
            .await
            .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "expired guard".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let outcome = repo
            .redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        assert!(outcome.successes.is_empty());
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].reason, "可兑换账号库存不足");

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert!(page.items[0].redeemed_at.is_none());
    }

    #[tokio::test]
    async fn redeem_refresh_candidates_are_only_selected_when_usable_stock_is_short() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("usable-1", "usable-access"),
            parsed_expired_account("expired-1", "expired-access-1"),
            parsed_expired_account("expired-2", "expired-access-2"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "refresh demand".to_string(),
                total_count: 2,
                accounts_per_code: 1,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let first_only = repo
            .prepare_redeem_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        assert_eq!(first_only.estimated_account_count, 1);
        assert!(first_only.refresh_account_ids.is_empty());

        let both = repo
            .prepare_redeem_export(&[batch.codes[0].code.clone(), batch.codes[1].code.clone()])
            .await
            .unwrap();
        assert_eq!(both.estimated_account_count, 2);
        assert_eq!(both.refresh_account_ids.len(), 1);
        let refreshed = repo
            .load_auth_files_for_ids(&both.refresh_account_ids, false)
            .await
            .unwrap()
            .into_iter()
            .map(|(summary, _)| summary.email.unwrap_or_default())
            .collect::<Vec<_>>();
        assert!(refreshed[0].starts_with("expired-"));
    }

    #[tokio::test]
    async fn duplicate_code_in_same_redeem_request_is_not_exported_twice() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("acct-1", "access-1")])
            .await
            .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "duplicate guard".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let outcome = repo
            .redeem_codes_for_export(
                &[batch.codes[0].code.clone(), batch.codes[0].code.clone()],
                ExportFormat::Cpa,
            )
            .await
            .unwrap();
        assert_eq!(outcome.successes.len(), 1);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].reason, "兑换码重复提交");
        assert_eq!(document_access_token(&outcome.document), "access-1");
    }

    #[tokio::test]
    async fn concurrent_redeems_do_not_allocate_the_same_account() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("acct-1", "access-1"),
            parsed_account("acct-2", "access-2"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "concurrent".to_string(),
                total_count: 2,
                accounts_per_code: 1,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let left_repo = repo.clone();
        let right_repo = repo.clone();
        let left_code = batch.codes[0].code.clone();
        let right_code = batch.codes[1].code.clone();
        let (left, right) = tokio::join!(
            async move {
                left_repo
                    .redeem_codes_for_export(&[left_code], ExportFormat::Cpa)
                    .await
            },
            async move {
                right_repo
                    .redeem_codes_for_export(&[right_code], ExportFormat::Cpa)
                    .await
            }
        );

        let left_token = document_access_token(&left.unwrap().document);
        let right_token = document_access_token(&right.unwrap().document);
        assert_ne!(left_token, right_token);

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(
            page.items
                .iter()
                .filter(|account| account.redeemed_at.is_some())
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn auto_probe_settings_are_persisted_and_normalized() {
        let repo = temp_repo().await;
        let default_settings = repo.get_auto_probe_settings().await.unwrap();
        assert!(!default_settings.enabled);
        assert_eq!(default_settings.interval_seconds, 60 * 60);
        assert_eq!(default_settings.max_accounts_per_run, 100);
        assert_eq!(default_settings.concurrency, 4);
        assert!(!default_settings.refresh_before_probe);
        assert!(!default_settings.proxy_enabled);
        assert_eq!(default_settings.proxy_mode, "fixed");
        assert_eq!(default_settings.proxy_default_scheme, "http");

        let saved = repo
            .save_auto_probe_settings(&AutoProbeSettings {
                enabled: true,
                interval_seconds: 1,
                max_accounts_per_run: 50_000,
                concurrency: 0,
                refresh_before_probe: false,
                proxy_enabled: true,
                proxy_mode: "711".to_string(),
                proxy_url: Some("  http://user:pass@proxy.example:10000  ".to_string()),
                proxy_api_url: Some("  https://api.example/proxies  ".to_string()),
                proxy_default_scheme: "socks".to_string(),
                last_started_at: Some(100),
                last_finished_at: Some(200),
                last_checked_count: 12,
                last_error: Some("previous error".to_string()),
                last_result: Some(json!({ "checked": 12 })),
                updated_at: 0,
            })
            .await
            .unwrap();

        assert!(saved.enabled);
        assert_eq!(saved.interval_seconds, 60);
        assert_eq!(saved.max_accounts_per_run, 5_000);
        assert_eq!(saved.concurrency, 1);
        assert!(!saved.refresh_before_probe);
        assert!(saved.proxy_enabled);
        assert_eq!(saved.proxy_mode, "api");
        assert_eq!(
            saved.proxy_url.as_deref(),
            Some("http://user:pass@proxy.example:10000")
        );
        assert_eq!(
            saved.proxy_api_url.as_deref(),
            Some("https://api.example/proxies")
        );
        assert_eq!(saved.proxy_default_scheme, "socks5");

        let loaded = repo.get_auto_probe_settings().await.unwrap();
        assert_eq!(loaded.interval_seconds, 60);
        assert_eq!(loaded.max_accounts_per_run, 5_000);
        assert_eq!(loaded.concurrency, 1);
        assert_eq!(loaded.last_checked_count, 12);
        assert_eq!(loaded.last_error.as_deref(), Some("previous error"));
        assert_eq!(loaded.last_result, Some(json!({ "checked": 12 })));
    }
}
