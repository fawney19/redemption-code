use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use account_pool_core::{
    access_token_needs_refresh, export_accounts, fingerprint_auth_file, format_redeem_code,
    generate_redeem_code, is_redeemed_account_deletable_status, legacy_fingerprint_auth_file,
    mask_redeem_code, normalize_redeem_code, redeem_code_hash, secret_preview, unix_now_secs,
    AccountStatus, CodexAuthFile, ExportFormat, HealthCheckResult, ParsedAccount,
    ACCESS_TOKEN_REFRESH_GRACE_SECONDS, REDEEMED_ACCOUNT_DELETABLE_STATUSES,
};
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

const INIT_SQL: &str = include_str!("../migrations/sqlite/0001_init.sql");
const AUTO_PROBE_SETTINGS_KEY: &str = "auto_probe";
const CPA_MANAGEMENT_KEY_SETTING_KEY: &str = "cpa_management_key";
const REDEEM_RATE_LIMIT_SETTINGS_KEY: &str = "redeem_rate_limit";
pub const DEFAULT_ACCOUNT_POOL_ID: &str = "default";
const DEFAULT_ACCOUNT_POOL_NAME: &str = "默认 Codex 号池";
const DEFAULT_ACCOUNT_POOL_WORKSPACE_LABEL: &str = "默认工作区";
const DEFAULT_ACCOUNT_POOL_TYPE: &str = "codex";
const DEFAULT_ACCOUNT_POOL_DESCRIPTION: &str = "旧账号和未指定池的默认归属";

#[derive(Debug, Error)]
pub enum DataError {
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("encryption error")]
    Encryption,
    #[error("invalid export format")]
    InvalidExportFormat,
    #[error("invalid input: {0}")]
    InvalidInput(String),
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

    pub async fn default_account_pool_id(&self) -> Result<String, DataError> {
        self.resolve_account_pool_id(None, false).await
    }

    pub async fn list_account_pools(&self) -> Result<Vec<AccountPoolSummary>, DataError> {
        let rows = sqlx::query(
            r#"
SELECT id, name, workspace_label, account_type, description, is_default, is_active, created_at, updated_at
FROM account_pools
ORDER BY is_default DESC, updated_at DESC, created_at DESC
"#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(account_pool_from_row).collect()
    }

    pub async fn create_account_pool(
        &self,
        input: AccountPoolUpsertInput,
    ) -> Result<AccountPoolSummary, DataError> {
        let input = input.normalized()?;
        let now = unix_now_secs() as i64;
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"
INSERT INTO account_pools (
  id, name, workspace_label, account_type, description, is_default, is_active, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, 0, ?, ?, ?)
"#,
        )
        .bind(&id)
        .bind(input.name)
        .bind(input.workspace_label)
        .bind(input.account_type)
        .bind(input.description)
        .bind(if input.is_active.unwrap_or(true) { 1_i64 } else { 0_i64 })
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.load_account_pool(&id).await
    }

    pub async fn update_account_pool(
        &self,
        pool_id: &str,
        input: AccountPoolUpsertInput,
    ) -> Result<AccountPoolSummary, DataError> {
        let pool_id = normalize_required_pool_id(pool_id)?;
        let existing = self.load_account_pool(&pool_id).await?;
        let input = input.normalized()?;
        let now = unix_now_secs() as i64;
        let is_active = if existing.is_default {
            true
        } else {
            input.is_active.unwrap_or(existing.is_active)
        };
        sqlx::query(
            r#"
UPDATE account_pools
SET name = ?, workspace_label = ?, account_type = ?, description = ?, is_active = ?, updated_at = ?
WHERE id = ?
"#,
        )
        .bind(input.name)
        .bind(input.workspace_label)
        .bind(input.account_type)
        .bind(input.description)
        .bind(if is_active { 1_i64 } else { 0_i64 })
        .bind(now)
        .bind(&pool_id)
        .execute(&self.pool)
        .await?;
        self.load_account_pool(&pool_id).await
    }

    async fn load_account_pool(&self, pool_id: &str) -> Result<AccountPoolSummary, DataError> {
        let row = sqlx::query(
            r#"
SELECT id, name, workspace_label, account_type, description, is_default, is_active, created_at, updated_at
FROM account_pools
WHERE id = ?
"#,
        )
        .bind(pool_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(account_pool_from_row)
            .transpose()?
            .ok_or(DataError::NotFound)
    }

    async fn resolve_account_pool_id(
        &self,
        pool_id: Option<&str>,
        require_active: bool,
    ) -> Result<String, DataError> {
        let pool_id = normalize_optional_pool_id(pool_id)
            .unwrap_or_else(|| DEFAULT_ACCOUNT_POOL_ID.to_string());
        let row = sqlx::query("SELECT is_active FROM account_pools WHERE id = ?")
            .bind(&pool_id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Err(DataError::InvalidInput("号池不存在".to_string()));
        };
        let is_active = row.try_get::<i64, _>("is_active").unwrap_or(0) != 0;
        if require_active && !is_active {
            return Err(DataError::InvalidInput("号池已停用".to_string()));
        }
        Ok(pool_id)
    }

    pub async fn import_accounts(
        &self,
        accounts: &[ParsedAccount],
    ) -> Result<ImportAccountsOutcome, DataError> {
        self.import_accounts_into_pool(accounts, None).await
    }

    pub async fn import_accounts_into_pool(
        &self,
        accounts: &[ParsedAccount],
        pool_id: Option<&str>,
    ) -> Result<ImportAccountsOutcome, DataError> {
        let pool_id = self.resolve_account_pool_id(pool_id, true).await?;
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
SET pool_id = CASE WHEN redeemed_at IS NULL THEN ? ELSE pool_id END,
    email = ?, name = ?, account_id = ?, plan_type = ?, auth_fingerprint = ?,
    auth_file_ciphertext = CASE WHEN redeemed_at IS NULL THEN ? ELSE auth_file_ciphertext END,
    access_token_preview = CASE WHEN redeemed_at IS NULL THEN ? ELSE access_token_preview END,
    refresh_token_preview = CASE WHEN redeemed_at IS NULL THEN ? ELSE refresh_token_preview END,
    expires_at = CASE WHEN redeemed_at IS NULL THEN ? ELSE expires_at END,
    status = CASE WHEN redeemed_at IS NULL THEN ? ELSE status END,
    updated_at = ?
WHERE id = ?
"#,
                )
                .bind(&pool_id)
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
  id, pool_id, email, name, account_id, plan_type, status, auth_fingerprint,
  auth_file_ciphertext, access_token_preview, refresh_token_preview,
  expires_at, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
                )
                .bind(Uuid::new_v4().to_string())
                .bind(&pool_id)
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
        let limit = if query.limit == 0 {
            50
        } else {
            query.limit.clamp(1, 500)
        };

        let mut count_builder =
            QueryBuilder::<Sqlite>::new("SELECT COUNT(*) AS count FROM accounts a");
        push_account_filters(&mut count_builder, &query);
        let count_row = count_builder.build().fetch_one(&self.pool).await?;
        let total = count_row
            .try_get::<i64, _>("count")
            .ok()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or_default();
        let offset = query.offset.min(total);

        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT a.id, a.pool_id, p.name AS pool_name, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.quota_snapshot, a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at
FROM accounts a
LEFT JOIN account_pools p ON p.id = a.pool_id
LEFT JOIN redeem_codes rc ON rc.id = a.redeem_code_id
"#,
        );
        push_account_filters(&mut builder, &query);
        builder
            .push(" ORDER BY a.created_at DESC, a.rowid DESC LIMIT ")
            .push_bind(limit as i64)
            .push(" OFFSET ")
            .push_bind(offset as i64);
        let rows = builder.build().fetch_all(&self.pool).await?;
        let items = rows
            .into_iter()
            .map(|row| account_summary_from_row(&row))
            .collect::<Result<Vec<_>, _>>()?;
        let stats = self
            .load_account_pool_stats(query.pool_id.as_deref())
            .await?;
        Ok(AccountListPage {
            items,
            total,
            limit,
            offset,
            stats,
        })
    }

    async fn load_account_pool_stats(
        &self,
        pool_id: Option<&str>,
    ) -> Result<AccountPoolStats, DataError> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT
  COUNT(*) AS total,
  COALESCE(SUM(CASE WHEN status = 'available' AND redeemed_at IS NULL THEN 1 ELSE 0 END), 0) AS available,
  COALESCE(SUM(CASE WHEN redeemed_at IS NOT NULL OR redeem_code_id IS NOT NULL OR redemption_id IS NOT NULL THEN 1 ELSE 0 END), 0) AS redeemed,
  COALESCE(SUM(CASE WHEN status IN ('at_expired', 'refresh_failed', 'auth_invalid', 'forbidden', 'quota_exhausted') THEN 1 ELSE 0 END), 0) AS attention
FROM accounts a
"#,
        );
        if let Some(pool_id) = normalize_optional_pool_id(pool_id) {
            builder.push(" WHERE a.pool_id = ").push_bind(pool_id);
        }
        let row = builder.build().fetch_one(&self.pool).await?;
        Ok(AccountPoolStats {
            total: usize_from_i64(row.try_get("total")?),
            available: usize_from_i64(row.try_get("available")?),
            redeemed: usize_from_i64(row.try_get("redeemed")?),
            attention: usize_from_i64(row.try_get("attention")?),
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
SELECT status, redeem_code_id, redemption_id, redeemed_at
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

            let status: String = row.try_get("status")?;
            let redeem_code_id: Option<String> = row.try_get("redeem_code_id")?;
            let redemption_id: Option<String> = row.try_get("redemption_id")?;
            let redeemed_at: Option<i64> = row.try_get("redeemed_at")?;
            let is_redeemed =
                redeem_code_id.is_some() || redemption_id.is_some() || redeemed_at.is_some();
            if is_redeemed && !is_redeemed_account_deletable_status(&status) {
                outcome.skipped += 1;
                outcome.results.push(DeleteAccountResult {
                    account_id: account_id.to_string(),
                    status: "skipped".to_string(),
                    reason: Some("账号已兑换且当前状态未失效，未删除".to_string()),
                });
                continue;
            }

            let mut delete_builder = QueryBuilder::<Sqlite>::new(
                r#"
DELETE FROM accounts
WHERE id =
"#,
            );
            delete_builder.push_bind(account_id);
            delete_builder.push(
                r#"
 AND (
  (redeem_code_id IS NULL AND redemption_id IS NULL AND redeemed_at IS NULL)
  OR (
    (redeem_code_id IS NOT NULL OR redemption_id IS NOT NULL OR redeemed_at IS NOT NULL)
    AND status IN (
"#,
            );
            {
                let mut separated = delete_builder.separated(", ");
                for status in REDEEMED_ACCOUNT_DELETABLE_STATUSES {
                    separated.push_bind(*status);
                }
                separated.push_unseparated(")))");
            }
            let deleted = delete_builder
                .build()
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
SELECT a.id, a.pool_id, p.name AS pool_name, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.quota_snapshot, a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at, a.auth_file_ciphertext
FROM accounts a
LEFT JOIN account_pools p ON p.id = a.pool_id
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
        self.load_unredeemed_auth_files_scoped(ids, None).await
    }

    pub async fn load_unredeemed_auth_files_scoped(
        &self,
        ids: Option<&[String]>,
        pool_id: Option<&str>,
    ) -> Result<Vec<(AccountSummary, CodexAuthFile)>, DataError> {
        if let Some(ids) = ids {
            return self.load_auth_files_for_ids(ids, false).await;
        }
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT a.id, a.pool_id, p.name AS pool_name, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.quota_snapshot, a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at, a.auth_file_ciphertext
FROM accounts a
LEFT JOIN account_pools p ON p.id = a.pool_id
LEFT JOIN redeem_codes rc ON rc.id = a.redeem_code_id
WHERE a.redeemed_at IS NULL
"#,
        );
        if let Some(pool_id) = normalize_optional_pool_id(pool_id) {
            builder.push(" AND a.pool_id = ").push_bind(pool_id);
        }
        builder.push(" ORDER BY a.created_at ASC");
        let rows = builder.build().fetch_all(&self.pool).await?;
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

    pub async fn update_redeemed_account_auth_snapshot(
        &self,
        account_id: &str,
        auth_file: &CodexAuthFile,
        refreshed_at: Option<u64>,
    ) -> Result<(), DataError> {
        let now = unix_now_secs() as i64;
        let mut tx = self.pool.begin().await?;
        let account_row = sqlx::query(
            r#"
SELECT redemption_id
FROM accounts
WHERE id = ? AND redeemed_at IS NOT NULL
"#,
        )
        .bind(account_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(account_row) = account_row else {
            tx.commit().await?;
            return Ok(());
        };

        sqlx::query(
            r#"
UPDATE accounts
SET auth_file_ciphertext = ?, access_token_preview = ?, refresh_token_preview = ?,
    expires_at = ?, last_refresh_at = COALESCE(?, last_refresh_at),
    updated_at = ?
WHERE id = ? AND redeemed_at IS NOT NULL
"#,
        )
        .bind(self.secrets.encrypt_json(&auth_file.clone().normalized())?)
        .bind(secret_preview(auth_file.access_token.as_deref()))
        .bind(secret_preview(auth_file.refresh_token.as_deref()))
        .bind(auth_file.expires_at_epoch().map(|value| value as i64))
        .bind(refreshed_at.map(|value| value as i64))
        .bind(now)
        .bind(account_id)
        .execute(&mut *tx)
        .await?;

        let redemption_id: Option<String> = account_row.try_get("redemption_id")?;
        let Some(redemption_id) = redemption_id.filter(|value| !value.trim().is_empty()) else {
            tx.commit().await?;
            return Ok(());
        };
        let Some(redemption_row) =
            sqlx::query("SELECT account_ids_json FROM redeem_redemptions WHERE id = ?")
                .bind(&redemption_id)
                .fetch_optional(&mut *tx)
                .await?
        else {
            tx.commit().await?;
            return Ok(());
        };
        let account_ids = serde_json::from_str::<Vec<String>>(
            redemption_row
                .try_get::<String, _>("account_ids_json")?
                .as_str(),
        )
        .unwrap_or_default();
        if account_ids.is_empty() {
            tx.commit().await?;
            return Ok(());
        }
        if let Some(auth_files) = self
            .load_existing_auth_snapshots_for_ids_tx(&mut tx, &account_ids)
            .await?
        {
            let snapshot = self.secrets.encrypt_json(&auth_files)?;
            sqlx::query(
                "UPDATE redeem_redemptions SET export_snapshot_ciphertext = ? WHERE id = ?",
            )
            .bind(snapshot)
            .bind(&redemption_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
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
            return Ok(AutoProbeSettings {
                cpa_management_key_set: self.cpa_management_key_set().await?,
                ..AutoProbeSettings::default()
            });
        };
        let value_json: String = row.try_get("value_json")?;
        let updated_at = optional_i64(&row, "updated_at")?.unwrap_or_default();
        let mut settings = serde_json::from_str::<AutoProbeSettings>(&value_json)
            .unwrap_or_else(|_| AutoProbeSettings::default());
        settings.updated_at = updated_at;
        let mut settings = settings.normalized();
        settings.cpa_management_key_set = self.cpa_management_key_set().await?;
        Ok(settings)
    }

    pub async fn save_auto_probe_settings(
        &self,
        settings: &AutoProbeSettings,
    ) -> Result<AutoProbeSettings, DataError> {
        let mut settings = settings.clone().normalized();
        settings.updated_at = unix_now_secs();
        let mut persisted = settings.clone();
        persisted.cpa_management_key_set = false;
        sqlx::query(
            r#"
INSERT INTO app_settings (key, value_json, updated_at)
VALUES (?, ?, ?)
ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at
"#,
        )
        .bind(AUTO_PROBE_SETTINGS_KEY)
        .bind(serde_json::to_string(&persisted).map_err(|_| DataError::Encryption)?)
        .bind(settings.updated_at as i64)
        .execute(&self.pool)
        .await?;
        settings.cpa_management_key_set = self.cpa_management_key_set().await?;
        Ok(settings)
    }

    pub async fn save_cpa_management_key(&self, management_key: &str) -> Result<(), DataError> {
        let management_key = management_key.trim();
        if management_key.is_empty() {
            return Ok(());
        }
        let now = unix_now_secs();
        let ciphertext = self.secrets.encrypt_json(&management_key.to_string())?;
        sqlx::query(
            r#"
INSERT INTO app_settings (key, value_json, updated_at)
VALUES (?, ?, ?)
ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at
"#,
        )
        .bind(CPA_MANAGEMENT_KEY_SETTING_KEY)
        .bind(serde_json::to_string(&ciphertext).map_err(|_| DataError::Encryption)?)
        .bind(now as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_cpa_management_key(&self) -> Result<Option<String>, DataError> {
        let row = sqlx::query("SELECT value_json FROM app_settings WHERE key = ?")
            .bind(CPA_MANAGEMENT_KEY_SETTING_KEY)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let value_json: String = row.try_get("value_json")?;
        let ciphertext =
            serde_json::from_str::<String>(&value_json).map_err(|_| DataError::Encryption)?;
        let value = self.secrets.decrypt_json::<String>(&ciphertext)?;
        Ok(Some(value).filter(|value| !value.trim().is_empty()))
    }

    pub async fn cpa_management_key_set(&self) -> Result<bool, DataError> {
        Ok(self.get_cpa_management_key().await?.is_some())
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
        self.create_redeem_batch_in_pool(input, None).await
    }

    pub async fn create_redeem_batch_in_pool(
        &self,
        input: CreateRedeemBatchInput,
        pool_id: Option<&str>,
    ) -> Result<CreateRedeemBatchOutcome, DataError> {
        let pool_id = self.resolve_account_pool_id(pool_id, true).await?;
        let now = unix_now_secs() as i64;
        let batch_id = Uuid::new_v4().to_string();
        let plan_filter_json = input
            .plan_filter
            .as_ref()
            .map(|value| json!(value).to_string());
        let after_sale_limit = input.after_sale_limit.unwrap_or(1).clamp(0, 10);
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
INSERT INTO redeem_code_batches (
  id, pool_id, name, status, total_count, redeemed_count, accounts_per_code, after_sale_limit,
  plan_filter_json, expires_at, created_at, updated_at
) VALUES (?, ?, ?, 'active', ?, 0, ?, ?, ?, ?, ?, ?)
"#,
        )
        .bind(&batch_id)
        .bind(&pool_id)
        .bind(input.name.trim())
        .bind(input.total_count as i64)
        .bind(input.accounts_per_code as i64)
        .bind(after_sale_limit as i64)
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
        self.list_redeem_batches_scoped(None).await
    }

    pub async fn list_redeem_batches_scoped(
        &self,
        pool_id: Option<&str>,
    ) -> Result<Vec<RedeemBatchSummary>, DataError> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT b.id, b.pool_id, p.name AS pool_name, b.name, b.status, b.total_count, b.redeemed_count, b.accounts_per_code, b.after_sale_limit,
       b.plan_filter_json, b.expires_at, b.created_at, b.updated_at
FROM redeem_code_batches b
LEFT JOIN account_pools p ON p.id = b.pool_id
"#,
        );
        if let Some(pool_id) = normalize_optional_pool_id(pool_id) {
            builder.push(" WHERE b.pool_id = ").push_bind(pool_id);
        }
        builder.push(" ORDER BY b.created_at DESC");
        let rows = builder.build().fetch_all(&self.pool).await?;
        rows.into_iter().map(batch_summary_from_row).collect()
    }

    pub async fn list_redeem_codes(
        &self,
        batch_id: &str,
    ) -> Result<Vec<RedeemCodeSummary>, DataError> {
        let rows = sqlx::query(
            r#"
SELECT codes.id, codes.batch_id, codes.masked_code, codes.code_ciphertext, codes.status,
       codes.redemption_id, codes.redeemed_at, codes.created_at, codes.updated_at,
       redemptions.account_ids_json
FROM redeem_codes AS codes
LEFT JOIN redeem_redemptions AS redemptions ON redemptions.id = codes.redemption_id
WHERE codes.batch_id = ?
ORDER BY codes.created_at ASC
"#,
        )
        .bind(batch_id)
        .fetch_all(&self.pool)
        .await?;
        let mut codes = rows
            .into_iter()
            .map(|row| code_summary_from_row(row, &self.secrets))
            .collect::<Result<Vec<_>, DataError>>()?;
        let code_ids = codes
            .iter()
            .map(|code| code.summary.id.clone())
            .collect::<Vec<_>>();
        let after_sale_map = self.load_after_sale_map(&code_ids).await?;
        let mut account_ids = codes
            .iter()
            .flat_map(|code| code.account_ids.iter().cloned())
            .collect::<Vec<_>>();
        account_ids.extend(after_sale_map.values().flatten().flat_map(|after_sale| {
            after_sale
                .old_account_ids
                .iter()
                .chain(after_sale.new_account_ids.iter())
                .cloned()
        }));
        let account_map = self.load_redeem_code_account_map(&account_ids).await?;
        for code in &mut codes {
            code.summary.accounts = code
                .account_ids
                .iter()
                .map(|account_id| {
                    account_map
                        .get(account_id)
                        .cloned()
                        .unwrap_or_else(|| deleted_redeem_code_account(account_id.clone()))
                })
                .collect();
            let after_sales = after_sale_map
                .get(&code.summary.id)
                .cloned()
                .unwrap_or_default();
            code.summary.after_sale_count = after_sales
                .iter()
                .filter(|after_sale| after_sale.summary.status == "success")
                .count() as u64;
            code.summary.after_sales =
                after_sales
                    .into_iter()
                    .map(|mut after_sale| {
                        after_sale.summary.old_accounts = after_sale
                            .old_account_ids
                            .iter()
                            .map(|account_id| {
                                account_map.get(account_id).cloned().unwrap_or_else(|| {
                                    deleted_redeem_code_account(account_id.clone())
                                })
                            })
                            .collect();
                        after_sale.summary.new_accounts = after_sale
                            .new_account_ids
                            .iter()
                            .map(|account_id| {
                                account_map.get(account_id).cloned().unwrap_or_else(|| {
                                    deleted_redeem_code_account(account_id.clone())
                                })
                            })
                            .collect();
                        after_sale.summary
                    })
                    .collect();
        }
        Ok(codes.into_iter().map(|code| code.summary).collect())
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
       batches.pool_id, batches.plan_filter_json, batches.expires_at
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
            let pool_id: String = row.try_get("pool_id")?;
            if accounts_per_code > 0 {
                estimated_account_count =
                    estimated_account_count.saturating_add(accounts_per_code as usize);
                demands.push(RedeemAccountDemand {
                    count: accounts_per_code as usize,
                    pool_id,
                    plan_filter,
                });
            }
        }
        if demands.is_empty() {
            return Ok(RedeemExportPreparation {
                estimated_account_count,
                refresh_account_ids: Vec::new(),
                probe_account_ids: Vec::new(),
            });
        }

        let rows = sqlx::query(
            r#"
SELECT id, pool_id, plan_type, status, expires_at
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
                    pool_id: row.try_get("pool_id")?,
                    plan_type: row.try_get("plan_type")?,
                    status: row.try_get("status")?,
                    expires_at: optional_i64(&row, "expires_at")?,
                })
            })
            .collect::<Result<Vec<_>, DataError>>()?;

        let mut selected_ids = HashSet::new();
        let mut refresh_ids = Vec::new();
        let mut probe_ids = Vec::new();
        for demand in demands {
            let mut remaining = demand.count;
            for candidate in &candidates {
                if remaining == 0 {
                    break;
                }
                if selected_ids.contains(&candidate.id) || !candidate.matches(&demand) {
                    continue;
                }
                if candidate.is_usable(now) {
                    selected_ids.insert(candidate.id.clone());
                    probe_ids.push(candidate.id.clone());
                    remaining -= 1;
                }
            }
            for candidate in &candidates {
                if remaining == 0 {
                    break;
                }
                if selected_ids.contains(&candidate.id) || !candidate.matches(&demand) {
                    continue;
                }
                if candidate.needs_refresh(now) {
                    selected_ids.insert(candidate.id.clone());
                    refresh_ids.push(candidate.id.clone());
                    probe_ids.push(candidate.id.clone());
                    remaining -= 1;
                }
            }
        }
        Ok(RedeemExportPreparation {
            estimated_account_count,
            refresh_account_ids: refresh_ids,
            probe_account_ids: probe_ids,
        })
    }

    pub async fn prepare_after_sale_export(
        &self,
        raw_codes: &[String],
    ) -> Result<RedeemAfterSalePreparation, DataError> {
        let now = unix_now_secs();
        let mut demands = Vec::new();
        let mut seen_hashes = HashSet::new();
        let mut seen_probe_ids = HashSet::new();
        let mut probe_ids = Vec::new();
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
SELECT codes.id AS code_id, codes.status AS code_status, codes.redemption_id,
       batches.status AS batch_status, batches.accounts_per_code,
       batches.pool_id, batches.plan_filter_json, batches.expires_at, batches.after_sale_limit,
       COALESCE((
         SELECT COUNT(*)
         FROM redeem_after_sales AS after_sales
         WHERE after_sales.code_id = codes.id AND after_sales.status = 'success'
       ), 0) AS after_sale_count
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
            let after_sale_limit: i64 = row.try_get("after_sale_limit")?;
            let after_sale_count: i64 = row.try_get("after_sale_count")?;
            if code_status == "disabled"
                || batch_status != "active"
                || expires_at.is_some_and(|value| value <= now as i64)
                || redemption_id.is_none()
                || after_sale_limit <= 0
                || after_sale_count >= after_sale_limit
            {
                continue;
            }

            let Some(redemption_id) = redemption_id else {
                continue;
            };
            if let Some(redemption_row) =
                sqlx::query("SELECT account_ids_json FROM redeem_redemptions WHERE id = ?")
                    .bind(redemption_id)
                    .fetch_optional(&self.pool)
                    .await?
            {
                let account_ids = serde_json::from_str::<Vec<String>>(
                    redemption_row
                        .try_get::<String, _>("account_ids_json")?
                        .as_str(),
                )
                .unwrap_or_default();
                for account_id in account_ids {
                    if seen_probe_ids.insert(account_id.clone()) {
                        probe_ids.push(account_id);
                    }
                }
            }

            let plan_filter = row
                .try_get::<Option<String>, _>("plan_filter_json")?
                .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                .unwrap_or_default();
            let accounts_per_code: i64 = row.try_get("accounts_per_code")?;
            let pool_id: String = row.try_get("pool_id")?;
            if accounts_per_code > 0 {
                estimated_account_count =
                    estimated_account_count.saturating_add(accounts_per_code as usize);
                demands.push(RedeemAccountDemand {
                    count: accounts_per_code as usize,
                    pool_id,
                    plan_filter,
                });
            }
        }

        let (refresh_ids, replacement_probe_ids) = self
            .select_replacement_candidates_for_demands(demands, now)
            .await?;
        for account_id in replacement_probe_ids {
            if seen_probe_ids.insert(account_id.clone()) {
                probe_ids.push(account_id);
            }
        }
        Ok(RedeemAfterSalePreparation {
            estimated_account_count,
            refresh_account_ids: refresh_ids,
            probe_account_ids: probe_ids,
        })
    }

    async fn select_replacement_candidates_for_demands(
        &self,
        demands: Vec<RedeemAccountDemand>,
        now: u64,
    ) -> Result<(Vec<String>, Vec<String>), DataError> {
        if demands.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let rows = sqlx::query(
            r#"
SELECT id, pool_id, plan_type, status, expires_at
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
                    pool_id: row.try_get("pool_id")?,
                    plan_type: row.try_get("plan_type")?,
                    status: row.try_get("status")?,
                    expires_at: optional_i64(&row, "expires_at")?,
                })
            })
            .collect::<Result<Vec<_>, DataError>>()?;

        let mut selected_ids = HashSet::new();
        let mut refresh_ids = Vec::new();
        let mut probe_ids = Vec::new();
        for demand in demands {
            let target_count = redeem_probe_target_count(demand.count);
            let mut selected_for_demand = 0_usize;
            for candidate in &candidates {
                if selected_for_demand >= target_count {
                    break;
                }
                if selected_ids.contains(&candidate.id) || !candidate.matches(&demand) {
                    continue;
                }
                if candidate.is_usable(now) {
                    selected_ids.insert(candidate.id.clone());
                    probe_ids.push(candidate.id.clone());
                    selected_for_demand += 1;
                }
            }
            for candidate in &candidates {
                if selected_for_demand >= demand.count {
                    break;
                }
                if selected_ids.contains(&candidate.id) || !candidate.matches(&demand) {
                    continue;
                }
                if candidate.needs_refresh(now) {
                    selected_ids.insert(candidate.id.clone());
                    refresh_ids.push(candidate.id.clone());
                    probe_ids.push(candidate.id.clone());
                    selected_for_demand += 1;
                }
            }
        }
        Ok((refresh_ids, probe_ids))
    }

    pub async fn redeem_codes_for_export(
        &self,
        raw_codes: &[String],
        format: ExportFormat,
    ) -> Result<RedeemExportOutcome, DataError> {
        self.redeem_codes_for_export_with_verified_accounts(raw_codes, format, None)
            .await
    }

    pub async fn redeem_codes_for_export_with_verified_accounts(
        &self,
        raw_codes: &[String],
        format: ExportFormat,
        verified_account_ids: Option<&[String]>,
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
       batches.accounts_per_code, batches.pool_id, batches.plan_filter_json, batches.expires_at
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
            let pool_id: String = code_row.try_get("pool_id")?;
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
                let mut account_query = QueryBuilder::<Sqlite>::new(
                    r#"
SELECT id, plan_type
FROM accounts
WHERE redeemed_at IS NULL AND status = 'available' AND pool_id =
"#,
                );
                account_query.push_bind(&pool_id);
                account_query.push(" AND expires_at IS NOT NULL AND expires_at > ");
                account_query.push_bind(usable_after);
                if let Some(verified_account_ids) = verified_account_ids {
                    if verified_account_ids.is_empty() {
                        account_query.push(" AND 1 = 0");
                    } else {
                        account_query.push(" AND id IN (");
                        let mut separated = account_query.separated(", ");
                        for account_id in verified_account_ids {
                            separated.push_bind(account_id);
                        }
                        separated.push_unseparated(")");
                    }
                }
                account_query.push(" ORDER BY created_at ASC");
                let rows = account_query.build().fetch_all(&mut *tx).await?;
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
                after_sale_count: None,
                replacement_account_count: None,
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

    pub async fn redeem_after_sale_for_export_with_verified_accounts(
        &self,
        raw_codes: &[String],
        format: ExportFormat,
        verified_current_account_ids: Option<&[String]>,
    ) -> Result<RedeemExportOutcome, DataError> {
        let _redeem_guard = self.redemption_lock.lock().await;
        let verified_current_account_ids = verified_current_account_ids.map(|ids| {
            ids.iter()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect::<HashSet<_>>()
        });
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
            let formatted_code = format_redeem_code(&normalized);
            let hash = redeem_code_hash(&normalized);
            if !seen_hashes.insert(hash.clone()) {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "兑换码重复提交".to_string(),
                });
                continue;
            }
            let Some(code_row) = sqlx::query(
                r#"
SELECT codes.id AS code_id, codes.batch_id, codes.status AS code_status,
       codes.redemption_id, batches.status AS batch_status,
       batches.accounts_per_code, batches.pool_id, batches.plan_filter_json, batches.expires_at,
       batches.after_sale_limit,
       COALESCE((
         SELECT COUNT(*)
         FROM redeem_after_sales AS after_sales
         WHERE after_sales.code_id = codes.id AND after_sales.status = 'success'
       ), 0) AS after_sale_count
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
                    code: formatted_code,
                    reason: "兑换码不存在".to_string(),
                });
                continue;
            };

            let code_id: String = code_row.try_get("code_id")?;
            let batch_id: String = code_row.try_get("batch_id")?;
            let code_status: String = code_row.try_get("code_status")?;
            let batch_status: String = code_row.try_get("batch_status")?;
            let pool_id: String = code_row.try_get("pool_id")?;
            let expires_at: Option<i64> = code_row.try_get("expires_at")?;
            if batch_status != "active" || code_status == "disabled" {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "兑换码已停用".to_string(),
                });
                continue;
            }
            if expires_at.is_some_and(|value| value <= now) {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "兑换码已过期".to_string(),
                });
                continue;
            }

            let Some(original_redemption_id) =
                code_row.try_get::<Option<String>, _>("redemption_id")?
            else {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "兑换码尚未兑换".to_string(),
                });
                continue;
            };
            let after_sale_limit: i64 = code_row.try_get("after_sale_limit")?;
            let after_sale_count: i64 = code_row.try_get("after_sale_count")?;
            if after_sale_limit <= 0 || after_sale_count >= after_sale_limit {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "该兑换码售后次数已用完".to_string(),
                });
                continue;
            }

            let Some(redemption_row) = sqlx::query(
                "SELECT account_ids_json, export_snapshot_ciphertext FROM redeem_redemptions WHERE id = ?",
            )
                    .bind(&original_redemption_id)
                    .fetch_optional(&mut *tx)
                    .await?
            else {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "兑换码尚未兑换".to_string(),
                });
                continue;
            };
            let old_account_ids = serde_json::from_str::<Vec<String>>(
                redemption_row
                    .try_get::<String, _>("account_ids_json")?
                    .as_str(),
            )
            .unwrap_or_default();
            if old_account_ids.is_empty() {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "当前绑定账号状态不支持自助售后".to_string(),
                });
                continue;
            }
            if let Some(verified_ids) = &verified_current_account_ids {
                if old_account_ids
                    .iter()
                    .any(|account_id| !verified_ids.contains(account_id))
                {
                    failures.push(RedeemFailure {
                        code: formatted_code,
                        reason: "售后测活失败，请稍后重试".to_string(),
                    });
                    continue;
                }
            }

            let old_statuses = load_account_statuses_tx(&mut tx, &old_account_ids).await?;
            if old_statuses.len() != old_account_ids.len() {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "当前绑定账号状态不支持自助售后".to_string(),
                });
                continue;
            }
            if old_account_ids.iter().all(|account_id| {
                old_statuses
                    .get(account_id)
                    .is_some_and(|status| status == AccountStatus::Available.as_str())
            }) {
                let snapshot_ciphertext: String =
                    redemption_row.try_get("export_snapshot_ciphertext")?;
                let auth_files = self
                    .secrets
                    .decrypt_json::<Vec<CodexAuthFile>>(&snapshot_ciphertext)?;
                successes.push(RedeemSuccess {
                    code: formatted_code,
                    account_count: old_account_ids.len(),
                    after_sale_count: Some(after_sale_count.max(0) as usize),
                    replacement_account_count: Some(0),
                });
                all_account_ids.extend(old_account_ids);
                all_auth_files.extend(auth_files.into_iter().map(CodexAuthFile::normalized));
                continue;
            }
            if old_account_ids.iter().any(|account_id| {
                old_statuses
                    .get(account_id)
                    .is_some_and(|status| status == AccountStatus::Available.as_str())
            }) {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "当前绑定账号仍可用".to_string(),
                });
                continue;
            }
            if old_account_ids.iter().any(|account_id| {
                old_statuses
                    .get(account_id)
                    .is_none_or(|status| !is_redeemed_account_deletable_status(status))
            }) {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "当前绑定账号状态不支持自助售后".to_string(),
                });
                continue;
            }

            let plan_filter = code_row
                .try_get::<Option<String>, _>("plan_filter_json")?
                .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                .unwrap_or_default();
            let accounts_per_code: i64 = code_row.try_get("accounts_per_code")?;
            let required_count = usize::try_from(accounts_per_code).unwrap_or_default();
            let mut account_query = QueryBuilder::<Sqlite>::new(
                r#"
SELECT id, plan_type
FROM accounts
WHERE redeemed_at IS NULL AND status = 'available' AND pool_id =
"#,
            );
            account_query.push_bind(&pool_id);
            account_query.push(" AND expires_at IS NOT NULL AND expires_at > ");
            account_query.push_bind(usable_after);
            if let Some(verified_ids) = &verified_current_account_ids {
                if verified_ids.is_empty() {
                    account_query.push(" AND 1 = 0");
                } else {
                    account_query.push(" AND id IN (");
                    let mut separated = account_query.separated(", ");
                    for account_id in verified_ids {
                        separated.push_bind(account_id);
                    }
                    separated.push_unseparated(")");
                }
            }
            account_query.push(" ORDER BY created_at ASC");
            let rows = account_query.build().fetch_all(&mut *tx).await?;
            let new_account_ids = rows
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
                .take(required_count)
                .collect::<Vec<_>>();
            if new_account_ids.len() < required_count {
                failures.push(RedeemFailure {
                    code: formatted_code,
                    reason: "可补发账号库存不足".to_string(),
                });
                continue;
            }

            let replacement_redemption_id = Uuid::new_v4().to_string();
            let auth_files = self
                .load_auth_files_for_ids_tx(&mut tx, &new_account_ids)
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
            .bind(&replacement_redemption_id)
            .bind(&code_id)
            .bind(&batch_id)
            .bind(format.as_str())
            .bind(serde_json::to_string(&new_account_ids).unwrap_or_else(|_| "[]".to_string()))
            .bind(&auth_files_snapshot)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            for account_id in &new_account_ids {
                let updated = sqlx::query(
                    r#"
UPDATE accounts
SET redeemed_at = ?, redeem_code_id = ?, redemption_id = ?, updated_at = ?
WHERE id = ? AND redeemed_at IS NULL
"#,
                )
                .bind(now)
                .bind(&code_id)
                .bind(&replacement_redemption_id)
                .bind(now)
                .bind(account_id)
                .execute(&mut *tx)
                .await?;
                if updated.rows_affected() != 1 {
                    return Err(DataError::NotFound);
                }
            }
            sqlx::query(
                "UPDATE redeem_codes SET status = 'redeemed', redemption_id = ?, updated_at = ? WHERE id = ?",
            )
            .bind(&replacement_redemption_id)
            .bind(now)
            .bind(&code_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                r#"
INSERT INTO redeem_after_sales (
  id, code_id, batch_id, original_redemption_id, replacement_redemption_id,
  old_account_ids_json, new_account_ids_json, export_format, export_snapshot_ciphertext,
  status, reason, created_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'success', ?, ?)
"#,
            )
            .bind(Uuid::new_v4().to_string())
            .bind(&code_id)
            .bind(&batch_id)
            .bind(&original_redemption_id)
            .bind(&replacement_redemption_id)
            .bind(serde_json::to_string(&old_account_ids).unwrap_or_else(|_| "[]".to_string()))
            .bind(serde_json::to_string(&new_account_ids).unwrap_or_else(|_| "[]".to_string()))
            .bind(format.as_str())
            .bind(&auth_files_snapshot)
            .bind("自动售后补发")
            .bind(now)
            .execute(&mut *tx)
            .await?;

            successes.push(RedeemSuccess {
                code: formatted_code,
                account_count: new_account_ids.len(),
                after_sale_count: Some((after_sale_count + 1).max(0) as usize),
                replacement_account_count: Some(new_account_ids.len()),
            });
            all_account_ids.extend(new_account_ids);
            all_auth_files.extend(auth_files);
        }

        let document = export_accounts(format, &all_auth_files);
        let export_id = Uuid::new_v4().to_string();
        let account_ids_json = json!(all_account_ids).to_string();
        sqlx::query(
            "INSERT INTO account_exports (id, format, source, account_ids_json, account_count, created_at) VALUES (?, ?, 'after_sale', ?, ?, ?)",
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
        self.export_admin_accounts_scoped(ids, include_redeemed, format, None)
            .await
    }

    pub async fn export_admin_accounts_scoped(
        &self,
        ids: Option<&[String]>,
        include_redeemed: bool,
        format: ExportFormat,
        pool_id: Option<&str>,
    ) -> Result<Value, DataError> {
        let accounts = if let Some(ids) = ids {
            self.load_auth_files_for_ids(ids, include_redeemed).await?
        } else {
            self.load_all_auth_files_scoped(pool_id)
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

    pub async fn get_redeem_rate_limit_settings(
        &self,
    ) -> Result<RedeemRateLimitSettings, DataError> {
        let row = sqlx::query("SELECT value_json, updated_at FROM app_settings WHERE key = ?")
            .bind(REDEEM_RATE_LIMIT_SETTINGS_KEY)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(RedeemRateLimitSettings::default());
        };
        let value_json: String = row.try_get("value_json")?;
        let updated_at = optional_i64(&row, "updated_at")?.unwrap_or_default();
        let mut settings = serde_json::from_str::<RedeemRateLimitSettings>(&value_json)
            .unwrap_or_else(|_| RedeemRateLimitSettings::default());
        settings.updated_at = updated_at;
        Ok(settings.normalized())
    }

    pub async fn save_redeem_rate_limit_settings(
        &self,
        settings: &RedeemRateLimitSettings,
    ) -> Result<RedeemRateLimitSettings, DataError> {
        let mut settings = settings.clone().normalized();
        settings.updated_at = unix_now_secs();
        sqlx::query(
            r#"
INSERT INTO app_settings (key, value_json, updated_at)
VALUES (?, ?, ?)
ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at
"#,
        )
        .bind(REDEEM_RATE_LIMIT_SETTINGS_KEY)
        .bind(serde_json::to_string(&settings).map_err(|_| DataError::Encryption)?)
        .bind(settings.updated_at as i64)
        .execute(&self.pool)
        .await?;
        Ok(settings)
    }

    async fn load_all_auth_files_scoped(
        &self,
        pool_id: Option<&str>,
    ) -> Result<Vec<(AccountSummary, CodexAuthFile)>, DataError> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT a.id, a.pool_id, p.name AS pool_name, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.quota_snapshot, a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at, a.auth_file_ciphertext
FROM accounts a
LEFT JOIN account_pools p ON p.id = a.pool_id
LEFT JOIN redeem_codes rc ON rc.id = a.redeem_code_id
"#,
        );
        if let Some(pool_id) = normalize_optional_pool_id(pool_id) {
            builder.push(" WHERE a.pool_id = ").push_bind(pool_id);
        }
        builder.push(" ORDER BY a.created_at ASC");
        let rows = builder.build().fetch_all(&self.pool).await?;
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
SELECT a.id, a.pool_id, p.name AS pool_name, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.quota_snapshot, a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at, a.auth_file_ciphertext
FROM accounts a
LEFT JOIN account_pools p ON p.id = a.pool_id
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

    async fn load_existing_auth_snapshots_for_ids_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        ids: &[String],
    ) -> Result<Option<Vec<CodexAuthFile>>, DataError> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let row = sqlx::query("SELECT auth_file_ciphertext FROM accounts WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
            let Some(row) = row else {
                return Ok(None);
            };
            let ciphertext: String = row.try_get("auth_file_ciphertext")?;
            out.push(
                self.secrets
                    .decrypt_json::<CodexAuthFile>(&ciphertext)?
                    .normalized(),
            );
        }
        Ok(Some(out))
    }

    async fn load_redeem_code_account_map(
        &self,
        account_ids: &[String],
    ) -> Result<HashMap<String, RedeemCodeAccountSummary>, DataError> {
        let mut unique_ids = Vec::new();
        let mut seen_ids = HashSet::new();
        for account_id in account_ids
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            if seen_ids.insert(account_id.to_string()) {
                unique_ids.push(account_id.to_string());
            }
        }
        if unique_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut out = HashMap::new();
        for chunk in unique_ids.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                r#"
SELECT a.id, a.pool_id, p.name AS pool_name, a.email, a.name, a.account_id, a.plan_type, a.status, a.last_probe_at, a.quota_snapshot
FROM accounts a
LEFT JOIN account_pools p ON p.id = a.pool_id
WHERE a.id IN (
"#,
            );
            {
                let mut separated = builder.separated(", ");
                for account_id in chunk {
                    separated.push_bind(account_id);
                }
                separated.push_unseparated(")");
            }
            let rows = builder.build().fetch_all(&self.pool).await?;
            for row in rows {
                let account = redeem_code_account_from_row(row)?;
                out.insert(account.id.clone(), account);
            }
        }
        Ok(out)
    }

    async fn load_after_sale_map(
        &self,
        code_ids: &[String],
    ) -> Result<HashMap<String, Vec<RedeemAfterSaleWithAccountIds>>, DataError> {
        let mut unique_ids = Vec::new();
        let mut seen_ids = HashSet::new();
        for code_id in code_ids
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            if seen_ids.insert(code_id.to_string()) {
                unique_ids.push(code_id.to_string());
            }
        }
        if unique_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut out: HashMap<String, Vec<RedeemAfterSaleWithAccountIds>> = HashMap::new();
        for chunk in unique_ids.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                r#"
SELECT id, code_id, old_account_ids_json, new_account_ids_json, status, reason, created_at
FROM redeem_after_sales
WHERE code_id IN (
"#,
            );
            {
                let mut separated = builder.separated(", ");
                for code_id in chunk {
                    separated.push_bind(code_id);
                }
                separated.push_unseparated(")");
            }
            builder.push(" ORDER BY created_at ASC");
            let rows = builder.build().fetch_all(&self.pool).await?;
            for row in rows {
                let code_id: String = row.try_get("code_id")?;
                let old_account_ids = row
                    .try_get::<String, _>("old_account_ids_json")
                    .ok()
                    .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                    .unwrap_or_default();
                let new_account_ids = row
                    .try_get::<String, _>("new_account_ids_json")
                    .ok()
                    .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                    .unwrap_or_default();
                out.entry(code_id.clone())
                    .or_default()
                    .push(RedeemAfterSaleWithAccountIds {
                        old_account_ids,
                        new_account_ids,
                        summary: RedeemAfterSaleSummary {
                            id: row.try_get("id")?,
                            status: row.try_get("status")?,
                            reason: row.try_get("reason")?,
                            old_accounts: Vec::new(),
                            new_accounts: Vec::new(),
                            created_at: optional_i64(&row, "created_at")?.unwrap_or_default(),
                        },
                    });
            }
        }
        Ok(out)
    }
}

async fn ensure_schema_upgrades(pool: &SqlitePool) -> Result<(), DataError> {
    ensure_default_account_pool(pool).await?;
    ensure_sqlite_column(
        pool,
        "accounts",
        "pool_id",
        "ALTER TABLE accounts ADD COLUMN pool_id TEXT NOT NULL DEFAULT 'default'",
    )
    .await?;
    ensure_sqlite_column(
        pool,
        "redeem_code_batches",
        "pool_id",
        "ALTER TABLE redeem_code_batches ADD COLUMN pool_id TEXT NOT NULL DEFAULT 'default'",
    )
    .await?;
    ensure_sqlite_column(
        pool,
        "redeem_codes",
        "code_ciphertext",
        "ALTER TABLE redeem_codes ADD COLUMN code_ciphertext TEXT",
    )
    .await?;
    ensure_sqlite_column(
        pool,
        "redeem_code_batches",
        "after_sale_limit",
        "ALTER TABLE redeem_code_batches ADD COLUMN after_sale_limit INTEGER NOT NULL DEFAULT 1",
    )
    .await?;
    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS redeem_after_sales (
  id TEXT PRIMARY KEY,
  code_id TEXT NOT NULL,
  batch_id TEXT NOT NULL,
  original_redemption_id TEXT NOT NULL,
  replacement_redemption_id TEXT NOT NULL,
  old_account_ids_json TEXT NOT NULL,
  new_account_ids_json TEXT NOT NULL,
  export_format TEXT NOT NULL,
  export_snapshot_ciphertext TEXT NOT NULL,
  status TEXT NOT NULL,
  reason TEXT,
  created_at INTEGER NOT NULL,
  FOREIGN KEY(code_id) REFERENCES redeem_codes(id) ON DELETE CASCADE,
  FOREIGN KEY(batch_id) REFERENCES redeem_code_batches(id) ON DELETE CASCADE,
  FOREIGN KEY(original_redemption_id) REFERENCES redeem_redemptions(id) ON DELETE CASCADE,
  FOREIGN KEY(replacement_redemption_id) REFERENCES redeem_redemptions(id) ON DELETE CASCADE
)
"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_redeem_after_sales_code ON redeem_after_sales(code_id, created_at)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_accounts_pool_status ON accounts(pool_id, status, updated_at)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_redeem_code_batches_pool ON redeem_code_batches(pool_id, status, created_at)",
    )
    .execute(pool)
    .await?;
    sqlx::query("UPDATE accounts SET pool_id = ? WHERE pool_id IS NULL OR trim(pool_id) = ''")
        .bind(DEFAULT_ACCOUNT_POOL_ID)
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE redeem_code_batches SET pool_id = ? WHERE pool_id IS NULL OR trim(pool_id) = ''",
    )
    .bind(DEFAULT_ACCOUNT_POOL_ID)
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_default_account_pool(pool: &SqlitePool) -> Result<(), DataError> {
    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS account_pools (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  workspace_label TEXT,
  account_type TEXT,
  description TEXT,
  is_default INTEGER NOT NULL DEFAULT 0,
  is_active INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
)
"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_account_pools_default ON account_pools(is_default) WHERE is_default = 1",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_account_pools_active ON account_pools(is_active, updated_at)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
INSERT OR IGNORE INTO account_pools (
  id, name, workspace_label, account_type, description, is_default, is_active, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, 1, 1, ?, ?)
"#,
    )
    .bind(DEFAULT_ACCOUNT_POOL_ID)
    .bind(DEFAULT_ACCOUNT_POOL_NAME)
    .bind(DEFAULT_ACCOUNT_POOL_WORKSPACE_LABEL)
    .bind(DEFAULT_ACCOUNT_POOL_TYPE)
    .bind(DEFAULT_ACCOUNT_POOL_DESCRIPTION)
    .bind(unix_now_secs() as i64)
    .bind(unix_now_secs() as i64)
    .execute(pool)
    .await?;
    Ok(())
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

#[derive(Debug, Clone, Serialize)]
pub struct AccountPoolSummary {
    pub id: String,
    pub name: String,
    pub workspace_label: Option<String>,
    pub account_type: Option<String>,
    pub description: Option<String>,
    pub is_default: bool,
    pub is_active: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountPoolUpsertInput {
    pub name: String,
    pub workspace_label: Option<String>,
    pub account_type: Option<String>,
    pub description: Option<String>,
    pub is_active: Option<bool>,
}

impl AccountPoolUpsertInput {
    fn normalized(self) -> Result<Self, DataError> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err(DataError::InvalidInput("号池名称不能为空".to_string()));
        }
        Ok(Self {
            name,
            workspace_label: normalize_optional_text(self.workspace_label),
            account_type: normalize_optional_text(self.account_type)
                .or_else(|| Some(DEFAULT_ACCOUNT_POOL_TYPE.to_string())),
            description: normalize_optional_text(self.description),
            is_active: self.is_active,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountListQuery {
    pub pool_id: Option<String>,
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
    pub stats: AccountPoolStats,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AccountPoolStats {
    pub total: usize,
    pub available: usize,
    pub redeemed: usize,
    pub attention: usize,
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
    pub pool_id: String,
    pub pool_name: Option<String>,
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
    pub quota_snapshot: Option<Value>,
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
    #[serde(default = "default_probe_mode")]
    pub probe_mode: String,
    #[serde(default = "default_deep_check_enabled")]
    pub deep_check_enabled: bool,
    #[serde(default)]
    pub cpa_base_url: Option<String>,
    #[serde(default)]
    pub cpa_management_key_set: bool,
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
            probe_mode: default_probe_mode(),
            deep_check_enabled: default_deep_check_enabled(),
            cpa_base_url: None,
            cpa_management_key_set: false,
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
        self.probe_mode = match self.probe_mode.trim().to_ascii_lowercase().as_str() {
            "direct" => "direct".to_string(),
            "cpa" => "cpa".to_string(),
            _ => "hybrid".to_string(),
        };
        self.cpa_base_url = self
            .cpa_base_url
            .map(|value| normalize_cpa_base_url(&value))
            .filter(|value| !value.is_empty());
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedeemRateLimitSettings {
    pub enabled: bool,
    pub window_seconds: u64,
    pub max_requests: u64,
    #[serde(default)]
    pub whitelist_ips: Vec<String>,
    pub updated_at: u64,
}

impl Default for RedeemRateLimitSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            window_seconds: 60,
            max_requests: 30,
            whitelist_ips: Vec::new(),
            updated_at: 0,
        }
    }
}

impl RedeemRateLimitSettings {
    pub fn normalized(mut self) -> Self {
        self.window_seconds = self.window_seconds.clamp(1, 24 * 60 * 60);
        self.max_requests = self.max_requests.clamp(1, 100_000);
        let mut seen = std::collections::HashSet::new();
        self.whitelist_ips = self
            .whitelist_ips
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .filter(|value| seen.insert(value.clone()))
            .collect();
        self
    }
}

fn default_probe_proxy_mode() -> String {
    "fixed".to_string()
}

fn default_probe_mode() -> String {
    "hybrid".to_string()
}

fn default_deep_check_enabled() -> bool {
    true
}

fn normalize_cpa_base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

fn default_probe_proxy_scheme() -> String {
    "http".to_string()
}

fn push_account_filters(builder: &mut QueryBuilder<'_, Sqlite>, query: &AccountListQuery) {
    let mut has_where = false;
    let mut push_and = |builder: &mut QueryBuilder<'_, Sqlite>| {
        if has_where {
            builder.push(" AND ");
        } else {
            builder.push(" WHERE ");
            has_where = true;
        }
    };

    if let Some(search) = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let pattern = format!("%{}%", search.to_ascii_lowercase());
        push_and(builder);
        builder
            .push("(lower(coalesce(a.email, '')) LIKE ")
            .push_bind(pattern.clone())
            .push(" OR lower(coalesce(a.name, '')) LIKE ")
            .push_bind(pattern.clone())
            .push(" OR lower(coalesce(a.account_id, '')) LIKE ")
            .push_bind(pattern.clone())
            .push(" OR lower(coalesce(a.plan_type, '')) LIKE ")
            .push_bind(pattern)
            .push(")");
    }
    if let Some(status) = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        push_and(builder);
        builder.push("a.status = ").push_bind(status.to_string());
    }
    if let Some(redeemed) = query.redeemed {
        push_and(builder);
        if redeemed {
            builder.push("a.redeemed_at IS NOT NULL");
        } else {
            builder.push("a.redeemed_at IS NULL");
        }
    }
    if let Some(pool_id) = normalize_optional_pool_id(query.pool_id.as_deref()) {
        push_and(builder);
        builder.push("a.pool_id = ").push_bind(pool_id);
    }
}

struct RedeemAccountDemand {
    count: usize,
    pool_id: String,
    plan_filter: Vec<String>,
}

fn redeem_probe_target_count(required: usize) -> usize {
    required.saturating_add(required.clamp(1, 10))
}

struct RedeemCandidateAccount {
    id: String,
    pool_id: String,
    plan_type: Option<String>,
    status: String,
    expires_at: Option<u64>,
}

impl RedeemCandidateAccount {
    fn matches(&self, demand: &RedeemAccountDemand) -> bool {
        self.pool_id == demand.pool_id
            && (demand.plan_filter.is_empty()
                || self.plan_type.as_ref().is_some_and(|value| {
                    demand
                        .plan_filter
                        .iter()
                        .any(|plan| plan.eq_ignore_ascii_case(value))
                }))
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
        pool_id: row
            .try_get("pool_id")
            .unwrap_or_else(|_| DEFAULT_ACCOUNT_POOL_ID.to_string()),
        pool_name: row.try_get("pool_name").ok(),
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
        quota_snapshot: optional_json(row, "quota_snapshot")?,
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

fn account_pool_from_row(row: sqlx::sqlite::SqliteRow) -> Result<AccountPoolSummary, DataError> {
    Ok(AccountPoolSummary {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        workspace_label: row.try_get("workspace_label")?,
        account_type: row.try_get("account_type")?,
        description: row.try_get("description")?,
        is_default: row.try_get::<i64, _>("is_default").unwrap_or(0) != 0,
        is_active: row.try_get::<i64, _>("is_active").unwrap_or(0) != 0,
        created_at: optional_i64(&row, "created_at")?.unwrap_or_default(),
        updated_at: optional_i64(&row, "updated_at")?.unwrap_or_default(),
    })
}

fn normalize_required_pool_id(value: &str) -> Result<String, DataError> {
    normalize_optional_pool_id(Some(value))
        .ok_or_else(|| DataError::InvalidInput("pool_id is required".to_string()))
}

fn normalize_optional_pool_id(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn optional_json(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<Option<Value>, DataError> {
    Ok(row
        .try_get::<Option<String>, _>(name)?
        .and_then(|value| serde_json::from_str::<Value>(&value).ok()))
}

async fn load_account_statuses_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_ids: &[String],
) -> Result<HashMap<String, String>, DataError> {
    if account_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new(
        r#"
SELECT id, status
FROM accounts
WHERE id IN (
"#,
    );
    {
        let mut separated = builder.separated(", ");
        for account_id in account_ids {
            separated.push_bind(account_id);
        }
        separated.push_unseparated(")");
    }
    let rows = builder.build().fetch_all(&mut **tx).await?;
    let mut out = HashMap::new();
    for row in rows {
        out.insert(row.try_get("id")?, row.try_get("status")?);
    }
    Ok(out)
}

fn usize_from_i64(value: i64) -> usize {
    usize::try_from(value).unwrap_or_default()
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRedeemBatchInput {
    pub name: String,
    pub total_count: usize,
    pub accounts_per_code: usize,
    #[serde(default)]
    pub after_sale_limit: Option<usize>,
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
    pub pool_id: String,
    pub pool_name: Option<String>,
    pub name: String,
    pub status: String,
    pub total_count: u64,
    pub redeemed_count: u64,
    pub accounts_per_code: u64,
    pub after_sale_limit: u64,
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
        pool_id: row
            .try_get("pool_id")
            .unwrap_or_else(|_| DEFAULT_ACCOUNT_POOL_ID.to_string()),
        pool_name: row.try_get("pool_name").ok(),
        name: row.try_get("name")?,
        status: row.try_get("status")?,
        total_count: optional_i64(&row, "total_count")?.unwrap_or_default(),
        redeemed_count: optional_i64(&row, "redeemed_count")?.unwrap_or_default(),
        accounts_per_code: optional_i64(&row, "accounts_per_code")?.unwrap_or_default(),
        after_sale_limit: optional_i64(&row, "after_sale_limit")?.unwrap_or(1),
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
    pub after_sale_count: u64,
    pub after_sales: Vec<RedeemAfterSaleSummary>,
    pub accounts: Vec<RedeemCodeAccountSummary>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemAfterSaleSummary {
    pub id: String,
    pub status: String,
    pub reason: Option<String>,
    pub old_accounts: Vec<RedeemCodeAccountSummary>,
    pub new_accounts: Vec<RedeemCodeAccountSummary>,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemCodeAccountSummary {
    pub id: String,
    pub pool_id: String,
    pub pool_name: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub account_id: Option<String>,
    pub plan_type: Option<String>,
    pub status: String,
    pub last_probe_at: Option<u64>,
    pub quota_snapshot: Option<Value>,
}

struct RedeemCodeWithAccountIds {
    summary: RedeemCodeSummary,
    account_ids: Vec<String>,
}

#[derive(Clone)]
struct RedeemAfterSaleWithAccountIds {
    summary: RedeemAfterSaleSummary,
    old_account_ids: Vec<String>,
    new_account_ids: Vec<String>,
}

fn code_summary_from_row(
    row: sqlx::sqlite::SqliteRow,
    secrets: &SecretBox,
) -> Result<RedeemCodeWithAccountIds, DataError> {
    let code = row
        .try_get::<Option<String>, _>("code_ciphertext")?
        .as_deref()
        .and_then(|ciphertext| secrets.decrypt_json::<String>(ciphertext).ok());
    let account_ids = row
        .try_get::<Option<String>, _>("account_ids_json")?
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default();
    Ok(RedeemCodeWithAccountIds {
        summary: RedeemCodeSummary {
            id: row.try_get("id")?,
            batch_id: row.try_get("batch_id")?,
            code,
            masked_code: row.try_get("masked_code")?,
            status: row.try_get("status")?,
            redemption_id: row.try_get("redemption_id")?,
            redeemed_at: optional_i64(&row, "redeemed_at")?,
            after_sale_count: 0,
            after_sales: Vec::new(),
            accounts: Vec::new(),
            created_at: optional_i64(&row, "created_at")?.unwrap_or_default(),
            updated_at: optional_i64(&row, "updated_at")?.unwrap_or_default(),
        },
        account_ids,
    })
}

fn redeem_code_account_from_row(
    row: sqlx::sqlite::SqliteRow,
) -> Result<RedeemCodeAccountSummary, DataError> {
    Ok(RedeemCodeAccountSummary {
        id: row.try_get("id")?,
        pool_id: row
            .try_get("pool_id")
            .unwrap_or_else(|_| DEFAULT_ACCOUNT_POOL_ID.to_string()),
        pool_name: row.try_get("pool_name").ok(),
        email: row.try_get("email")?,
        name: row.try_get("name")?,
        account_id: row.try_get("account_id")?,
        plan_type: row.try_get("plan_type")?,
        status: row.try_get("status")?,
        last_probe_at: optional_i64(&row, "last_probe_at")?,
        quota_snapshot: optional_json(&row, "quota_snapshot")?,
    })
}

fn deleted_redeem_code_account(account_id: String) -> RedeemCodeAccountSummary {
    RedeemCodeAccountSummary {
        id: account_id,
        pool_id: DEFAULT_ACCOUNT_POOL_ID.to_string(),
        pool_name: Some(DEFAULT_ACCOUNT_POOL_NAME.to_string()),
        email: None,
        name: None,
        account_id: None,
        plan_type: None,
        status: "deleted".to_string(),
        last_probe_at: None,
        quota_snapshot: None,
    }
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
    pub probe_account_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemAfterSalePreparation {
    pub estimated_account_count: usize,
    pub refresh_account_ids: Vec<String>,
    pub probe_account_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemSuccess {
    pub code: String,
    pub account_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_sale_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_account_count: Option<usize>,
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

    async fn set_account_status(
        repo: &AccountPoolRepository,
        account_id: &str,
        status: AccountStatus,
    ) {
        repo.record_health_check(
            account_id,
            &HealthCheckResult {
                status,
                plan_type: Some("plus".to_string()),
                quota_snapshot: None,
                error: Some(format!("status set to {}", status.as_str())),
            },
            Some(200),
            Some(1),
        )
        .await
        .unwrap();
    }

    fn pool_input(name: &str) -> AccountPoolUpsertInput {
        AccountPoolUpsertInput {
            name: name.to_string(),
            workspace_label: Some(name.to_string()),
            account_type: Some("codex".to_string()),
            description: None,
            is_active: Some(true),
        }
    }

    #[tokio::test]
    async fn startup_creates_default_pool_and_backfills_legacy_rows() {
        let path = std::env::temp_dir().join(format!(
            "aether-pool-legacy-test-{}.sqlite3",
            Uuid::new_v4()
        ));
        let database_url = format!("sqlite://{}", path.display());
        let options = SqliteConnectOptions::from_str(&database_url)
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query(
            r#"
CREATE TABLE accounts (
  id TEXT PRIMARY KEY,
  email TEXT,
  name TEXT,
  account_id TEXT,
  plan_type TEXT,
  status TEXT NOT NULL DEFAULT 'available',
  auth_fingerprint TEXT NOT NULL UNIQUE,
  auth_file_ciphertext TEXT NOT NULL,
  access_token_preview TEXT,
  refresh_token_preview TEXT,
  expires_at INTEGER,
  last_refresh_at INTEGER,
  last_probe_at INTEGER,
  quota_snapshot TEXT,
  redeem_code_id TEXT,
  redemption_id TEXT,
  redeemed_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
)
"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
CREATE TABLE redeem_code_batches (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'active',
  total_count INTEGER NOT NULL,
  redeemed_count INTEGER NOT NULL DEFAULT 0,
  accounts_per_code INTEGER NOT NULL,
  after_sale_limit INTEGER NOT NULL DEFAULT 1,
  plan_filter_json TEXT,
  expires_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
)
"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO accounts (id, auth_fingerprint, auth_file_ciphertext, created_at, updated_at) VALUES ('legacy-account', 'legacy-fp', 'legacy-cipher', 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO redeem_code_batches (id, name, total_count, accounts_per_code, created_at, updated_at) VALUES ('legacy-batch', 'legacy', 1, 1, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let pools = repo.list_account_pools().await.unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].id, DEFAULT_ACCOUNT_POOL_ID);
        assert!(pools[0].is_default);

        let account_pool_id: String =
            sqlx::query("SELECT pool_id FROM accounts WHERE id = 'legacy-account'")
                .fetch_one(repo.pool())
                .await
                .unwrap()
                .try_get("pool_id")
                .unwrap();
        let batch_pool_id: String =
            sqlx::query("SELECT pool_id FROM redeem_code_batches WHERE id = 'legacy-batch'")
                .fetch_one(repo.pool())
                .await
                .unwrap()
                .try_get("pool_id")
                .unwrap();
        assert_eq!(account_pool_id, DEFAULT_ACCOUNT_POOL_ID);
        assert_eq!(batch_pool_id, DEFAULT_ACCOUNT_POOL_ID);
    }

    #[tokio::test]
    async fn pools_scope_account_listing_stats_and_admin_exports() {
        let repo = temp_repo().await;
        let left = repo.create_account_pool(pool_input("left")).await.unwrap();
        let right = repo.create_account_pool(pool_input("right")).await.unwrap();
        repo.import_accounts_into_pool(
            &[parsed_account("left-acct", "left-access")],
            Some(&left.id),
        )
        .await
        .unwrap();
        repo.import_accounts_into_pool(
            &[parsed_account("right-acct", "right-access")],
            Some(&right.id),
        )
        .await
        .unwrap();

        let left_page = repo
            .list_accounts(AccountListQuery {
                pool_id: Some(left.id.clone()),
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(left_page.total, 1);
        assert_eq!(left_page.stats.total, 1);
        assert_eq!(left_page.items[0].pool_id, left.id);

        let right_export = repo
            .export_admin_accounts_scoped(None, false, ExportFormat::Sub2api, Some(&right.id))
            .await
            .unwrap();
        let accounts = right_export
            .get("accounts")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(
            accounts[0]
                .get("credentials")
                .and_then(|value| value.get("access_token"))
                .and_then(Value::as_str),
            Some("right-access")
        );
    }

    #[tokio::test]
    async fn redeem_and_after_sale_replacements_stay_in_batch_pool() {
        let repo = temp_repo().await;
        let left = repo.create_account_pool(pool_input("left")).await.unwrap();
        let right = repo.create_account_pool(pool_input("right")).await.unwrap();
        repo.import_accounts_into_pool(
            &[
                parsed_account("left-old", "left-old-access"),
                parsed_account("left-fresh", "left-fresh-access"),
            ],
            Some(&left.id),
        )
        .await
        .unwrap();
        repo.import_accounts_into_pool(
            &[parsed_account("right-fresh", "right-fresh-access")],
            Some(&right.id),
        )
        .await
        .unwrap();

        let right_only_batch = repo
            .create_redeem_batch_in_pool(
                CreateRedeemBatchInput {
                    name: "left-empty".to_string(),
                    total_count: 1,
                    accounts_per_code: 3,
                    after_sale_limit: None,
                    expires_at: None,
                    plan_filter: None,
                },
                Some(&left.id),
            )
            .await
            .unwrap();
        let insufficient = repo
            .redeem_codes_for_export(&[right_only_batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        assert!(insufficient.successes.is_empty());
        assert_eq!(insufficient.failures[0].reason, "可兑换账号库存不足");

        let batch = repo
            .create_redeem_batch_in_pool(
                CreateRedeemBatchInput {
                    name: "left-after-sale".to_string(),
                    total_count: 1,
                    accounts_per_code: 1,
                    after_sale_limit: Some(1),
                    expires_at: None,
                    plan_filter: None,
                },
                Some(&left.id),
            )
            .await
            .unwrap();
        let original = repo
            .redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        assert_eq!(document_access_token(&original.document), "left-old-access");
        let page = repo
            .list_accounts(AccountListQuery {
                pool_id: Some(left.id.clone()),
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let old_account = page
            .items
            .iter()
            .find(|account| account.email.as_deref() == Some("left-old@example.com"))
            .unwrap()
            .clone();
        set_account_status(&repo, &old_account.id, AccountStatus::AuthInvalid).await;
        let prep = repo
            .prepare_after_sale_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        let after_sale = repo
            .redeem_after_sale_for_export_with_verified_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                Some(&prep.probe_account_ids),
            )
            .await
            .unwrap();
        assert_eq!(
            document_access_token(&after_sale.document),
            "left-fresh-access"
        );
    }

    #[tokio::test]
    async fn reimport_moves_only_unredeemed_accounts_between_pools() {
        let repo = temp_repo().await;
        let left = repo.create_account_pool(pool_input("left")).await.unwrap();
        let right = repo.create_account_pool(pool_input("right")).await.unwrap();
        repo.import_accounts_into_pool(&[parsed_account("moving", "left-access")], Some(&left.id))
            .await
            .unwrap();
        repo.import_accounts_into_pool(
            &[parsed_account("moving", "right-access")],
            Some(&right.id),
        )
        .await
        .unwrap();
        let moved = repo
            .list_accounts(AccountListQuery {
                pool_id: Some(right.id.clone()),
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(moved.total, 1);
        assert_eq!(moved.items[0].pool_id, right.id);

        let batch = repo
            .create_redeem_batch_in_pool(
                CreateRedeemBatchInput {
                    name: "redeemed".to_string(),
                    total_count: 1,
                    accounts_per_code: 1,
                    after_sale_limit: None,
                    expires_at: None,
                    plan_filter: None,
                },
                Some(&right.id),
            )
            .await
            .unwrap();
        repo.redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        repo.import_accounts_into_pool(&[parsed_account("moving", "left-again")], Some(&left.id))
            .await
            .unwrap();
        let all = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(all.total, 1);
        assert_eq!(all.items[0].pool_id, right.id);
        assert!(all.items[0].redeemed_at.is_some());
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
    async fn list_accounts_uses_database_pagination_and_filters() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("acct-a", "access-a"),
            parsed_account("acct-b", "access-b"),
            parsed_account("acct-c", "access-c"),
        ])
        .await
        .unwrap();

        let first_page = repo
            .list_accounts(AccountListQuery {
                limit: 2,
                offset: 0,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(first_page.total, 3);
        assert_eq!(first_page.items.len(), 2);
        assert_eq!(first_page.stats.total, 3);
        assert_eq!(first_page.stats.available, 3);
        assert_eq!(first_page.stats.redeemed, 0);
        assert_eq!(first_page.stats.attention, 0);

        let second_page = repo
            .list_accounts(AccountListQuery {
                limit: 2,
                offset: 2,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(second_page.total, 3);
        assert_eq!(second_page.items.len(), 1);

        let filtered = repo
            .list_accounts(AccountListQuery {
                search: Some("acct-b@example.com".to_string()),
                limit: 50,
                offset: 0,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(filtered.total, 1);
        assert_eq!(filtered.stats.total, 3);
        assert_eq!(filtered.stats.available, 3);
        assert_eq!(
            filtered.items[0].email.as_deref(),
            Some("acct-b@example.com")
        );
    }

    #[tokio::test]
    async fn list_accounts_includes_quota_snapshot() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("quota-acct", "quota-access")])
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
                quota_snapshot: Some(json!({
                    "primary_used_percent": 12.5,
                    "secondary_used_percent": 34.0,
                })),
                error: None,
            },
            Some(200),
            Some(30),
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
        let snapshot = page.items[0].quota_snapshot.as_ref().unwrap();
        assert_eq!(snapshot.get("primary_used_percent"), Some(&json!(12.5)));
        assert_eq!(snapshot.get("secondary_used_percent"), Some(&json!(34.0)));
    }

    #[tokio::test]
    async fn health_check_does_not_move_account_to_top_of_list() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("stable-a", "access-a"),
            parsed_account("stable-b", "access-b"),
            parsed_account("stable-c", "access-c"),
        ])
        .await
        .unwrap();

        let before = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let before_ids = before
            .items
            .iter()
            .map(|account| account.id.clone())
            .collect::<Vec<_>>();
        let checked_id = before_ids[1].clone();

        repo.record_health_check(
            &checked_id,
            &HealthCheckResult {
                status: AccountStatus::Available,
                plan_type: Some("plus".to_string()),
                quota_snapshot: Some(json!({ "primary_used_percent": 5.0 })),
                error: None,
            },
            Some(200),
            Some(25),
        )
        .await
        .unwrap();

        let after = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let after_ids = after
            .items
            .iter()
            .map(|account| account.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(after_ids, before_ids);
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
    async fn delete_skips_redeemed_accounts_that_are_not_invalid() {
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
                after_sale_limit: None,
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
    async fn delete_allows_redeemed_invalid_accounts() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("acct-1", "access-1")])
            .await
            .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "delete invalid redeemed".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: None,
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
        let redeemed = page.items[0].id.clone();

        repo.record_health_check(
            &redeemed,
            &HealthCheckResult {
                status: AccountStatus::AuthInvalid,
                plan_type: Some("plus".to_string()),
                quota_snapshot: None,
                error: Some("invalid token".to_string()),
            },
            Some(401),
            Some(36),
        )
        .await
        .unwrap();

        let outcome = repo
            .delete_unbound_accounts(std::slice::from_ref(&redeemed))
            .await
            .unwrap();
        assert_eq!(outcome.deleted, 1);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.results[0].status, "deleted");

        let remaining = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(remaining.total, 0);
        let row =
            sqlx::query("SELECT COUNT(*) AS count FROM redeem_codes WHERE redeemed_at IS NOT NULL")
                .fetch_one(repo.pool())
                .await
                .unwrap();
        let redeemed_codes: i64 = row.try_get("count").unwrap();
        assert_eq!(redeemed_codes, 1);
    }

    #[tokio::test]
    async fn list_redeem_codes_returns_full_codes_for_new_batches() {
        let repo = temp_repo().await;
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "full code export".to_string(),
                total_count: 3,
                accounts_per_code: 1,
                after_sale_limit: None,
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
        assert!(codes.iter().all(|code| code.accounts.is_empty()));
    }

    #[tokio::test]
    async fn list_redeem_codes_includes_bound_accounts() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("bound-acct", "bound-access")])
            .await
            .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "bound accounts".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        repo.redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        let codes = repo.list_redeem_codes(&batch.batch_id).await.unwrap();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].accounts.len(), 1);
        assert_eq!(
            codes[0].accounts[0].email.as_deref(),
            Some("bound-acct@example.com")
        );
        assert_eq!(
            codes[0].accounts[0].status,
            AccountStatus::Available.as_str()
        );
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
                after_sale_limit: None,
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
                after_sale_limit: None,
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
                after_sale_limit: None,
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
        assert_eq!(first_only.probe_account_ids.len(), 1);

        let both = repo
            .prepare_redeem_export(&[batch.codes[0].code.clone(), batch.codes[1].code.clone()])
            .await
            .unwrap();
        assert_eq!(both.estimated_account_count, 2);
        assert_eq!(both.refresh_account_ids.len(), 1);
        assert_eq!(both.probe_account_ids.len(), 2);
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
    async fn redeem_with_verified_scope_does_not_use_unverified_available_accounts() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("acct-1", "access-1"),
            parsed_account("acct-2", "access-2"),
        ])
        .await
        .unwrap();
        let accounts = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let verified_id = accounts
            .items
            .iter()
            .find(|account| account.email.as_deref() == Some("acct-2@example.com"))
            .unwrap()
            .id
            .clone();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "verified scope".to_string(),
                total_count: 2,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let blocked = repo
            .redeem_codes_for_export_with_verified_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                Some(&[]),
            )
            .await
            .unwrap();
        assert!(blocked.successes.is_empty());
        assert_eq!(blocked.failures[0].reason, "可兑换账号库存不足");

        let scoped = repo
            .redeem_codes_for_export_with_verified_accounts(
                &[batch.codes[1].code.clone()],
                ExportFormat::Cpa,
                Some(&[verified_id]),
            )
            .await
            .unwrap();
        assert_eq!(scoped.successes.len(), 1);
        assert_eq!(document_access_token(&scoped.document), "access-2");
    }

    #[tokio::test]
    async fn after_sale_replaces_invalid_current_binding_and_keeps_original_redemption_history() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("old-1", "old-access"),
            parsed_account("fresh-1", "fresh-access"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "after sale success".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: Some(1),
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let original = repo
            .redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        assert_eq!(document_access_token(&original.document), "old-access");

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let old_account = page
            .items
            .iter()
            .find(|account| account.email.as_deref() == Some("old-1@example.com"))
            .unwrap()
            .clone();
        let original_redemption_id = old_account.redemption_id.clone().unwrap();

        set_account_status(&repo, &old_account.id, AccountStatus::AuthInvalid).await;

        let preparation = repo
            .prepare_after_sale_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        assert_eq!(preparation.estimated_account_count, 1);
        assert_eq!(preparation.refresh_account_ids.len(), 0);
        assert!(preparation.probe_account_ids.contains(&old_account.id));

        let after_sale = repo
            .redeem_after_sale_for_export_with_verified_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                Some(&preparation.probe_account_ids),
            )
            .await
            .unwrap();
        assert_eq!(after_sale.successes.len(), 1);
        assert_eq!(after_sale.successes[0].account_count, 1);
        assert_eq!(after_sale.successes[0].after_sale_count, Some(1));
        assert_eq!(document_access_token(&after_sale.document), "fresh-access");

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let old_account = page
            .items
            .iter()
            .find(|account| account.email.as_deref() == Some("old-1@example.com"))
            .unwrap();
        let new_account = page
            .items
            .iter()
            .find(|account| account.email.as_deref() == Some("fresh-1@example.com"))
            .unwrap();
        assert_eq!(
            old_account.redemption_id.as_deref(),
            Some(original_redemption_id.as_str())
        );
        assert!(new_account.redemption_id.is_some());
        assert_eq!(old_account.status, AccountStatus::AuthInvalid.as_str());

        let codes = repo.list_redeem_codes(&batch.batch_id).await.unwrap();
        assert_eq!(codes[0].after_sale_count, 1);
        assert_eq!(codes[0].after_sales.len(), 1);
        assert_eq!(codes[0].after_sales[0].old_accounts.len(), 1);
        assert_eq!(codes[0].after_sales[0].new_accounts.len(), 1);
        assert_eq!(
            codes[0].after_sales[0].new_accounts[0].email.as_deref(),
            Some("fresh-1@example.com")
        );
    }

    #[tokio::test]
    async fn after_sale_reexports_refreshed_current_binding_without_consuming_reissue() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("old-1", "old-access")])
            .await
            .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "after sale restored current".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: Some(1),
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let original = repo
            .redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        assert_eq!(document_access_token(&original.document), "old-access");

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let old_account = page
            .items
            .iter()
            .find(|account| account.email.as_deref() == Some("old-1@example.com"))
            .unwrap()
            .clone();
        let refreshed_auth = parsed_account("old-1", "old-access-refreshed").auth_file;
        repo.update_redeemed_account_auth_snapshot(&old_account.id, &refreshed_auth, Some(123))
            .await
            .unwrap();
        set_account_status(&repo, &old_account.id, AccountStatus::Available).await;

        let after_sale = repo
            .redeem_after_sale_for_export_with_verified_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                Some(std::slice::from_ref(&old_account.id)),
            )
            .await
            .unwrap();
        assert_eq!(after_sale.successes.len(), 1);
        assert!(after_sale.failures.is_empty());
        assert_eq!(after_sale.successes[0].account_count, 1);
        assert_eq!(after_sale.successes[0].after_sale_count, Some(0));
        assert_eq!(after_sale.successes[0].replacement_account_count, Some(0));
        assert_eq!(
            document_access_token(&after_sale.document),
            "old-access-refreshed"
        );

        let repeat_export = repo
            .redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        assert_eq!(
            document_access_token(&repeat_export.document),
            "old-access-refreshed"
        );
        let codes = repo.list_redeem_codes(&batch.batch_id).await.unwrap();
        assert_eq!(codes[0].after_sale_count, 0);
        assert!(codes[0].after_sales.is_empty());
    }

    #[tokio::test]
    async fn after_sale_reexports_when_current_binding_is_still_available() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("old-1", "old-access"),
            parsed_account("fresh-1", "fresh-access"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "after sale available".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: Some(1),
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        repo.redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();

        let outcome = repo
            .redeem_after_sale_for_export_with_verified_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome.successes.len(), 1);
        assert!(outcome.failures.is_empty());
        assert_eq!(outcome.successes[0].after_sale_count, Some(0));
        assert_eq!(outcome.successes[0].replacement_account_count, Some(0));
        assert_eq!(document_access_token(&outcome.document), "old-access");
    }

    #[tokio::test]
    async fn after_sale_rejects_quota_exhausted_binding() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("old-1", "old-access")])
            .await
            .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "after sale quota".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: Some(1),
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
        let old_account = page.items[0].clone();
        set_account_status(&repo, &old_account.id, AccountStatus::QuotaExhausted).await;

        let outcome = repo
            .redeem_after_sale_for_export_with_verified_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                None,
            )
            .await
            .unwrap();
        assert!(outcome.successes.is_empty());
        assert_eq!(outcome.failures[0].reason, "当前绑定账号状态不支持自助售后");
    }

    #[tokio::test]
    async fn after_sale_limit_zero_blocks_reissue() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("old-1", "old-access"),
            parsed_account("fresh-1", "fresh-access"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "after sale limit zero".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: Some(0),
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
        let old_account = page
            .items
            .iter()
            .find(|account| account.email.as_deref() == Some("old-1@example.com"))
            .unwrap()
            .clone();
        set_account_status(&repo, &old_account.id, AccountStatus::AuthInvalid).await;

        let outcome = repo
            .redeem_after_sale_for_export_with_verified_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                None,
            )
            .await
            .unwrap();
        assert!(outcome.successes.is_empty());
        assert_eq!(outcome.failures[0].reason, "该兑换码售后次数已用完");
    }

    #[tokio::test]
    async fn after_sale_does_not_reuse_same_account_for_concurrent_reissue() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("old-1", "old-access"),
            parsed_account("old-2", "old-access-2"),
            parsed_account("fresh-1", "fresh-access-1"),
            parsed_account("fresh-2", "fresh-access-2"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "after sale concurrent".to_string(),
                total_count: 2,
                accounts_per_code: 1,
                after_sale_limit: Some(1),
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        repo.redeem_codes_for_export(&[batch.codes[0].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        repo.redeem_codes_for_export(&[batch.codes[1].code.clone()], ExportFormat::Cpa)
            .await
            .unwrap();
        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let old_1 = page
            .items
            .iter()
            .find(|account| account.email.as_deref() == Some("old-1@example.com"))
            .unwrap()
            .id
            .clone();
        let old_2 = page
            .items
            .iter()
            .find(|account| account.email.as_deref() == Some("old-2@example.com"))
            .unwrap()
            .id
            .clone();
        set_account_status(&repo, &old_1, AccountStatus::AuthInvalid).await;
        set_account_status(&repo, &old_2, AccountStatus::AuthInvalid).await;
        let prep = repo
            .prepare_after_sale_export(&[batch.codes[0].code.clone(), batch.codes[1].code.clone()])
            .await
            .unwrap();
        assert_eq!(prep.probe_account_ids.len(), 4);

        let left_repo = repo.clone();
        let right_repo = repo.clone();
        let left_code = batch.codes[0].code.clone();
        let right_code = batch.codes[1].code.clone();
        let left_verified_ids = prep.probe_account_ids.clone();
        let right_verified_ids = prep.probe_account_ids.clone();
        let (left, right) = tokio::join!(
            async move {
                left_repo
                    .redeem_after_sale_for_export_with_verified_accounts(
                        &[left_code],
                        ExportFormat::Cpa,
                        Some(&left_verified_ids),
                    )
                    .await
            },
            async move {
                right_repo
                    .redeem_after_sale_for_export_with_verified_accounts(
                        &[right_code],
                        ExportFormat::Cpa,
                        Some(&right_verified_ids),
                    )
                    .await
            }
        );
        let left = left.unwrap();
        let right = right.unwrap();
        assert_eq!(left.successes.len() + right.successes.len(), 2);
        let left_token = document_access_token(&left.document);
        let right_token = document_access_token(&right.document);
        assert_ne!(left_token, right_token);
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
                after_sale_limit: None,
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
                after_sale_limit: None,
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
                probe_mode: "711".to_string(),
                deep_check_enabled: true,
                cpa_base_url: Some(" https://cpa.example/ ".to_string()),
                cpa_management_key_set: true,
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
        assert_eq!(saved.probe_mode, "hybrid");
        assert!(saved.deep_check_enabled);
        assert_eq!(saved.cpa_base_url.as_deref(), Some("https://cpa.example"));
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

        assert!(!loaded.cpa_management_key_set);
        repo.save_cpa_management_key("secret-cpa-key")
            .await
            .unwrap();
        let with_key = repo.get_auto_probe_settings().await.unwrap();
        assert!(with_key.cpa_management_key_set);
        assert_eq!(
            repo.get_cpa_management_key().await.unwrap().as_deref(),
            Some("secret-cpa-key")
        );
    }

    #[tokio::test]
    async fn redeem_rate_limit_settings_are_persisted_and_normalized() {
        let repo = temp_repo().await;
        let defaults = repo.get_redeem_rate_limit_settings().await.unwrap();
        assert!(defaults.enabled);
        assert_eq!(defaults.window_seconds, 60);
        assert_eq!(defaults.max_requests, 30);

        let saved = repo
            .save_redeem_rate_limit_settings(&RedeemRateLimitSettings {
                enabled: true,
                window_seconds: 0,
                max_requests: 0,
                whitelist_ips: vec![
                    "  203.0.113.10  ".to_string(),
                    "203.0.113.10".to_string(),
                    "".to_string(),
                    "2001:db8::1".to_string(),
                ],
                updated_at: 0,
            })
            .await
            .unwrap();
        assert_eq!(saved.window_seconds, 1);
        assert_eq!(saved.max_requests, 1);
        assert_eq!(
            saved.whitelist_ips,
            vec!["203.0.113.10".to_string(), "2001:db8::1".to_string()]
        );

        let loaded = repo.get_redeem_rate_limit_settings().await.unwrap();
        assert_eq!(loaded.window_seconds, 1);
        assert_eq!(loaded.max_requests, 1);
        assert_eq!(loaded.whitelist_ips.len(), 2);
    }
}
