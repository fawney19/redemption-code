use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::domain::{
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
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
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
const REDEEM_RESERVATION_TTL_SECONDS: u64 = 30 * 60;

#[derive(Debug, Error)]
pub enum DataError {
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("encryption error")]
    Encryption,
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
        let max_connections = env_u32_clamped("AETHER_POOL_DB_MAX_CONNECTIONS", 8, 1, 64);
        let busy_timeout_ms = env_u64_clamped("AETHER_POOL_DB_BUSY_TIMEOUT_MS", 30_000, 0, 600_000);
        let options = SqliteConnectOptions::from_str(database_url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_millis(busy_timeout_ms));
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
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

    #[cfg(test)]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn release_redeem_reservation(&self, reservation_id: &str) -> Result<(), DataError> {
        let reservation_id = reservation_id.trim();
        if reservation_id.is_empty() {
            return Ok(());
        }
        sqlx::query(
            r#"
UPDATE accounts
SET redeem_reservation_id = NULL, redeem_reserved_at = NULL, updated_at = ?
WHERE redeem_reservation_id = ?
"#,
        )
        .bind(unix_now_secs() as i64)
        .bind(reservation_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn refresh_redeem_reservation(&self, reservation_id: &str) -> Result<(), DataError> {
        let reservation_id = reservation_id.trim();
        if reservation_id.is_empty() {
            return Ok(());
        }
        let now = unix_now_secs() as i64;
        sqlx::query(
            r#"
UPDATE accounts
SET redeem_reserved_at = ?, updated_at = ?
WHERE redeem_reservation_id = ? AND redeemed_at IS NULL
"#,
        )
        .bind(now)
        .bind(now)
        .bind(reservation_id)
        .execute(&self.pool)
        .await?;
        Ok(())
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
        let mut pools = rows
            .into_iter()
            .map(account_pool_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        for pool in &mut pools {
            pool.stats = self.load_account_pool_stats(Some(&pool.id)).await?;
        }
        Ok(pools)
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
        let mut pool = row
            .map(account_pool_from_row)
            .transpose()?
            .ok_or(DataError::NotFound)?;
        pool.stats = self.load_account_pool_stats(Some(&pool.id)).await?;
        Ok(pool)
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

    #[cfg(test)]
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
        let mut prepared = Vec::with_capacity(accounts.len());
        let mut imported = 0;
        let mut updated = 0;

        for (source_index, parsed) in accounts.iter().enumerate() {
            let auth_file = parsed.auth_file.clone().normalized();
            let fingerprint = fingerprint_auth_file(&auth_file);
            let ciphertext = self.secrets.encrypt_json(&auth_file)?;
            let expires_at = auth_file.expires_at_epoch().map(|value| value as i64);
            let status = if expires_at.is_some_and(|value| value <= now) {
                AccountStatus::AtExpired
            } else {
                AccountStatus::Available
            }
            .as_str()
            .to_string();
            let email_key = auth_file
                .email
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_ascii_lowercase);
            let legacy_fingerprint = legacy_fingerprint_auth_file(&auth_file);
            let external_account_id = auth_file
                .account_id
                .clone()
                .or_else(|| auth_file.chatgpt_account_id.clone());
            prepared.push(PreparedImportAccount {
                source_index,
                auth_file,
                fingerprint,
                legacy_fingerprint,
                email_key,
                ciphertext,
                expires_at,
                status,
                external_account_id,
            });
        }

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let fingerprints = prepared
            .iter()
            .map(|item| item.fingerprint.clone())
            .collect::<Vec<_>>();
        let existing_by_fingerprint =
            load_import_ids_by_fingerprint_tx(&mut tx, &fingerprints).await?;
        let legacy_keys = prepared
            .iter()
            .filter(|item| !existing_by_fingerprint.contains_key(&item.fingerprint))
            .filter_map(|item| {
                let email = item.email_key.as_ref()?;
                if item.legacy_fingerprint == item.fingerprint {
                    return None;
                }
                Some((item.legacy_fingerprint.clone(), email.clone()))
            })
            .collect::<Vec<_>>();
        let existing_by_legacy = load_import_ids_by_legacy_tx(&mut tx, &legacy_keys).await?;
        let mut results = Vec::with_capacity(prepared.len());

        for item in prepared {
            let exists = existing_by_fingerprint
                .get(&item.fingerprint)
                .cloned()
                .or_else(|| {
                    let email = item.email_key.as_ref()?;
                    existing_by_legacy
                        .get(&(item.legacy_fingerprint.clone(), email.clone()))
                        .cloned()
                });
            if let Some(id) = exists {
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
                .bind(&item.auth_file.email)
                .bind(&item.auth_file.name)
                .bind(
                    item.auth_file
                        .account_id
                        .clone()
                        .or_else(|| item.auth_file.chatgpt_account_id.clone()),
                )
                .bind(
                    item.auth_file
                        .plan_type
                        .clone()
                        .or_else(|| item.auth_file.chatgpt_plan_type.clone()),
                )
                .bind(&item.fingerprint)
                .bind(&item.ciphertext)
                .bind(secret_preview(item.auth_file.access_token.as_deref()))
                .bind(secret_preview(item.auth_file.refresh_token.as_deref()))
                .bind(item.expires_at)
                .bind(&item.status)
                .bind(now)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
                updated += 1;
                results.push(ImportAccountResult {
                    source_index: item.source_index,
                    id: Some(id),
                    email: item.auth_file.email,
                    external_account_id: item.external_account_id,
                    pool_id: pool_id.clone(),
                    action: "updated".to_string(),
                    status: Some(item.status),
                    error: None,
                });
            } else {
                let id = Uuid::new_v4().to_string();
                sqlx::query(
                    r#"
INSERT INTO accounts (
  id, pool_id, email, name, account_id, plan_type, status, auth_fingerprint,
  auth_file_ciphertext, access_token_preview, refresh_token_preview,
  expires_at, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
                )
                .bind(&id)
                .bind(&pool_id)
                .bind(&item.auth_file.email)
                .bind(&item.auth_file.name)
                .bind(
                    item.auth_file
                        .account_id
                        .clone()
                        .or_else(|| item.auth_file.chatgpt_account_id.clone()),
                )
                .bind(
                    item.auth_file
                        .plan_type
                        .clone()
                        .or_else(|| item.auth_file.chatgpt_plan_type.clone()),
                )
                .bind(&item.status)
                .bind(&item.fingerprint)
                .bind(&item.ciphertext)
                .bind(secret_preview(item.auth_file.access_token.as_deref()))
                .bind(secret_preview(item.auth_file.refresh_token.as_deref()))
                .bind(item.expires_at)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                imported += 1;
                results.push(ImportAccountResult {
                    source_index: item.source_index,
                    id: Some(id),
                    email: item.auth_file.email,
                    external_account_id: item.external_account_id,
                    pool_id: pool_id.clone(),
                    action: "imported".to_string(),
                    status: Some(item.status),
                    error: None,
                });
            }
        }
        tx.commit().await?;
        Ok(ImportAccountsOutcome {
            imported,
            updated,
            results,
        })
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

    pub async fn list_account_ids(
        &self,
        query: AccountListQuery,
    ) -> Result<Vec<String>, DataError> {
        let mut builder = QueryBuilder::<Sqlite>::new("SELECT a.id FROM accounts a");
        push_account_filters(&mut builder, &query);
        builder.push(" ORDER BY a.created_at ASC, a.rowid ASC");
        let rows = builder.build().fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| row.try_get("id").map_err(DataError::from))
            .collect()
    }

    async fn load_account_pool_stats(
        &self,
        pool_id: Option<&str>,
    ) -> Result<AccountPoolStats, DataError> {
        let usable_after =
            (unix_now_secs() as i64).saturating_add(ACCESS_TOKEN_REFRESH_GRACE_SECONDS as i64);
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT
  COUNT(*) AS total,
  COALESCE(SUM(CASE WHEN status = 'available' AND redeemed_at IS NULL
    AND expires_at IS NOT NULL AND expires_at > "#,
        );
        builder.push_bind(usable_after);
        builder.push(
            r#" THEN 1 ELSE 0 END), 0) AS available,
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

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
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut unique_ids = Vec::new();
        let mut seen_ids = HashSet::new();
        for id in ids {
            if seen_ids.insert(id.clone()) {
                unique_ids.push(id.clone());
            }
        }

        let mut pairs = HashMap::new();
        for chunk in unique_ids.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                r#"
SELECT a.id, a.pool_id, p.name AS pool_name, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.quota_snapshot, a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at, a.auth_file_ciphertext
FROM accounts a
LEFT JOIN account_pools p ON p.id = a.pool_id
LEFT JOIN redeem_codes rc ON rc.id = a.redeem_code_id
WHERE a.id IN (
"#,
            );
            {
                let mut separated = builder.separated(", ");
                for id in chunk {
                    separated.push_bind(id);
                }
                separated.push_unseparated(")");
            }
            if !include_redeemed {
                builder.push(" AND a.redeemed_at IS NULL");
            }
            let rows = builder.build().fetch_all(&self.pool).await?;
            for row in rows {
                let pair = self.auth_pair_from_row(row)?;
                pairs.insert(pair.0.id.clone(), pair);
            }
        }

        let mut out = Vec::new();
        for id in ids {
            if let Some((summary, auth_file)) = pairs.get(id) {
                out.push((summary.clone(), auth_file.clone()));
            }
        }
        Ok(out)
    }

    #[cfg(test)]
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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

    #[cfg(test)]
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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

        struct RedeemCodeCandidate {
            id: String,
            hash: String,
            prefix: String,
            suffix: String,
            masked_code: String,
            code_ciphertext: String,
            created: RedeemCodeCreated,
        }

        let mut codes = Vec::with_capacity(input.total_count);
        let mut seen_hashes = HashSet::new();
        while codes.len() < input.total_count {
            let remaining = input.total_count - codes.len();
            let target_count = remaining.min(500);
            let mut candidates = Vec::with_capacity(target_count);
            while candidates.len() < target_count {
                let formatted = generate_redeem_code();
                let Some(normalized) = normalize_redeem_code(&formatted) else {
                    continue;
                };
                let hash = redeem_code_hash(&normalized);
                if !seen_hashes.insert(hash.clone()) {
                    continue;
                }
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
                candidates.push(RedeemCodeCandidate {
                    id: code_id.clone(),
                    hash,
                    prefix,
                    suffix,
                    masked_code: masked_code.clone(),
                    code_ciphertext,
                    created: RedeemCodeCreated {
                        id: code_id,
                        code,
                        masked_code,
                    },
                });
            }

            let mut insert = QueryBuilder::<Sqlite>::new(
                r#"
INSERT OR IGNORE INTO redeem_codes (
  id, batch_id, code_hash, code_prefix, code_suffix, masked_code, code_ciphertext,
  created_at, updated_at
)
"#,
            );
            insert.push_values(&candidates, |mut row, candidate| {
                row.push_bind(&candidate.id)
                    .push_bind(&batch_id)
                    .push_bind(&candidate.hash)
                    .push_bind(&candidate.prefix)
                    .push_bind(&candidate.suffix)
                    .push_bind(&candidate.masked_code)
                    .push_bind(&candidate.code_ciphertext)
                    .push_bind(now)
                    .push_bind(now);
            });
            let inserted = insert.build().execute(&mut *tx).await?;
            if inserted.rows_affected() == candidates.len() as u64 {
                codes.extend(candidates.into_iter().map(|candidate| candidate.created));
                continue;
            }

            let mut id_query =
                QueryBuilder::<Sqlite>::new("SELECT id FROM redeem_codes WHERE batch_id = ");
            id_query.push_bind(&batch_id);
            id_query.push(" AND id IN (");
            {
                let mut separated = id_query.separated(", ");
                for candidate in &candidates {
                    separated.push_bind(&candidate.id);
                }
                separated.push_unseparated(")");
            }
            let inserted_ids = id_query
                .build()
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .map(|row| row.try_get::<String, _>("id"))
                .collect::<Result<HashSet<_>, _>>()?;
            codes.extend(candidates.into_iter().filter_map(|candidate| {
                inserted_ids
                    .contains(&candidate.id)
                    .then_some(candidate.created)
            }));
        }
        tx.commit().await?;
        Ok(CreateRedeemBatchOutcome { batch_id, codes })
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

    pub async fn delete_redeem_batch(
        &self,
        batch_id: &str,
    ) -> Result<DeleteRedeemBatchOutcome, DataError> {
        let batch_id = batch_id.trim();
        if batch_id.is_empty() {
            return Err(DataError::InvalidInput("batch_id is required".to_string()));
        }

        let _redeem_guard = self.redemption_lock.lock().await;
        let now = unix_now_secs() as i64;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let Some(row) = sqlx::query(
            r#"
SELECT
  b.id,
  (SELECT COUNT(*) FROM redeem_codes WHERE batch_id = b.id) AS code_count,
  (SELECT COUNT(*) FROM redeem_redemptions WHERE batch_id = b.id) AS redemption_count,
  (SELECT COUNT(*) FROM redeem_after_sales WHERE batch_id = b.id) AS after_sale_count
FROM redeem_code_batches b
WHERE b.id = ?
"#,
        )
        .bind(batch_id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.rollback().await?;
            return Ok(DeleteRedeemBatchOutcome {
                deleted: false,
                accounts_reset: 0,
                codes_deleted: 0,
                redemptions_deleted: 0,
                after_sales_deleted: 0,
            });
        };

        let codes_deleted = optional_i64(&row, "code_count")?.unwrap_or_default() as usize;
        let redemptions_deleted =
            optional_i64(&row, "redemption_count")?.unwrap_or_default() as usize;
        let after_sales_deleted =
            optional_i64(&row, "after_sale_count")?.unwrap_or_default() as usize;

        let accounts_reset = sqlx::query(
            r#"
UPDATE accounts
SET redeem_code_id = NULL,
    redemption_id = NULL,
    redeemed_at = NULL,
    updated_at = ?
WHERE EXISTS (
    SELECT 1
    FROM redeem_codes c
    WHERE c.batch_id = ? AND c.id = accounts.redeem_code_id
  )
  OR EXISTS (
    SELECT 1
    FROM redeem_redemptions r
    WHERE r.batch_id = ? AND r.id = accounts.redemption_id
  )
"#,
        )
        .bind(now)
        .bind(batch_id)
        .bind(batch_id)
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize;

        let deleted = sqlx::query("DELETE FROM redeem_code_batches WHERE id = ?")
            .bind(batch_id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;

        tx.commit().await?;
        Ok(DeleteRedeemBatchOutcome {
            deleted,
            accounts_reset,
            codes_deleted,
            redemptions_deleted,
            after_sales_deleted,
        })
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

    pub async fn estimate_redeem_export(
        &self,
        raw_codes: &[String],
    ) -> Result<RedeemExportPreparation, DataError> {
        self.prepare_redeem_export_inner(raw_codes, false).await
    }

    pub async fn prepare_redeem_export(
        &self,
        raw_codes: &[String],
    ) -> Result<RedeemExportPreparation, DataError> {
        self.prepare_redeem_export_inner(raw_codes, true).await
    }

    async fn prepare_redeem_export_inner(
        &self,
        raw_codes: &[String],
        reserve_accounts: bool,
    ) -> Result<RedeemExportPreparation, DataError> {
        struct CodePreparation {
            code_status: String,
            batch_status: String,
            accounts_per_code: usize,
            pool_id: String,
            plan_filter: Vec<String>,
            expires_at: Option<i64>,
            redemption_id: Option<String>,
        }

        let now = unix_now_secs();
        let mut hashes = Vec::new();
        let mut seen_hashes = HashSet::new();
        for raw_code in raw_codes {
            let Some(normalized) = normalize_redeem_code(raw_code) else {
                continue;
            };
            let hash = redeem_code_hash(&normalized);
            if seen_hashes.insert(hash.clone()) {
                hashes.push(hash);
            }
        }

        let mut tx = if reserve_accounts {
            self.pool.begin_with("BEGIN IMMEDIATE").await?
        } else {
            self.pool.begin().await?
        };
        let reservation_cutoff = redeem_reservation_cutoff(now);
        if reserve_accounts {
            clear_expired_redeem_reservations_tx(&mut tx, reservation_cutoff).await?;
        }

        let mut code_rows = HashMap::new();
        for chunk in hashes.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                r#"
SELECT codes.code_hash, codes.status AS code_status, codes.redemption_id,
       batches.status AS batch_status, batches.accounts_per_code,
       batches.pool_id, batches.plan_filter_json, batches.expires_at
FROM redeem_codes AS codes
JOIN redeem_code_batches AS batches ON batches.id = codes.batch_id
WHERE codes.code_hash IN (
"#,
            );
            {
                let mut separated = builder.separated(", ");
                for hash in chunk {
                    separated.push_bind(hash);
                }
                separated.push_unseparated(")");
            }
            let rows = builder.build().fetch_all(&mut *tx).await?;
            for row in rows {
                let hash: String = row.try_get("code_hash")?;
                let plan_filter = row
                    .try_get::<Option<String>, _>("plan_filter_json")?
                    .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                    .unwrap_or_default();
                code_rows.insert(
                    hash,
                    CodePreparation {
                        code_status: row.try_get("code_status")?,
                        batch_status: row.try_get("batch_status")?,
                        accounts_per_code: usize_from_i64(row.try_get("accounts_per_code")?),
                        pool_id: row.try_get("pool_id")?,
                        plan_filter,
                        expires_at: row.try_get("expires_at")?,
                        redemption_id: row.try_get("redemption_id")?,
                    },
                );
            }
        }

        let redemption_ids = hashes
            .iter()
            .filter_map(|hash| {
                let row = code_rows.get(hash)?;
                if row.code_status != "active"
                    || row.batch_status != "active"
                    || row.expires_at.is_some_and(|value| value <= now as i64)
                {
                    return None;
                }
                row.redemption_id.clone()
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut redemption_account_counts = HashMap::new();
        for chunk in redemption_ids.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                "SELECT id, account_ids_json FROM redeem_redemptions WHERE id IN (",
            );
            {
                let mut separated = builder.separated(", ");
                for redemption_id in chunk {
                    separated.push_bind(redemption_id);
                }
                separated.push_unseparated(")");
            }
            let rows = builder.build().fetch_all(&mut *tx).await?;
            for row in rows {
                let redemption_id: String = row.try_get("id")?;
                let account_ids = serde_json::from_str::<Vec<String>>(
                    row.try_get::<String, _>("account_ids_json")?.as_str(),
                )
                .unwrap_or_default();
                redemption_account_counts.insert(redemption_id, account_ids.len());
            }
        }

        let mut demands = Vec::new();
        let mut estimated_account_count = 0_usize;
        for hash in hashes {
            let Some(row) = code_rows.get(&hash) else {
                continue;
            };
            if row.code_status != "active"
                || row.batch_status != "active"
                || row.expires_at.is_some_and(|value| value <= now as i64)
            {
                continue;
            }
            if let Some(redemption_id) = &row.redemption_id {
                if let Some(account_count) = redemption_account_counts.get(redemption_id) {
                    estimated_account_count =
                        estimated_account_count.saturating_add(*account_count);
                }
                continue;
            }
            if row.accounts_per_code > 0 {
                estimated_account_count =
                    estimated_account_count.saturating_add(row.accounts_per_code);
                demands.push(RedeemAccountDemand {
                    count: row.accounts_per_code,
                    pool_id: row.pool_id.clone(),
                    plan_filter: row.plan_filter.clone(),
                });
            }
        }
        if demands.is_empty() {
            tx.commit().await?;
            return Ok(RedeemExportPreparation {
                estimated_account_count,
                refresh_account_ids: Vec::new(),
                probe_account_ids: Vec::new(),
                reservation_id: None,
            });
        }

        let demand_pool_ids = demands
            .iter()
            .map(|demand| demand.pool_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT id, pool_id, plan_type, status, expires_at
FROM accounts
WHERE redeemed_at IS NULL AND status IN ('available', 'at_expired')
  AND (redeem_reservation_id IS NULL OR redeem_reserved_at IS NULL OR redeem_reserved_at <= 
"#,
        );
        builder.push_bind(reservation_cutoff);
        builder.push(")");
        builder.push(" AND pool_id IN (");
        {
            let mut separated = builder.separated(", ");
            for pool_id in &demand_pool_ids {
                separated.push_bind(pool_id);
            }
            separated.push_unseparated(")");
        }
        builder.push(" ORDER BY created_at ASC");
        let rows = builder.build().fetch_all(&mut *tx).await?;
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
        let mut reserved_ids = Vec::new();
        for demand in demands {
            let target_count = if reserve_accounts {
                redeem_probe_target_count(demand.count)
            } else {
                demand.count
            };
            let mut selected_for_demand = 0_usize;
            for candidate in &candidates {
                if selected_for_demand >= target_count {
                    break;
                }
                if selected_ids.contains(&candidate.id) || !candidate.matches(&demand) {
                    continue;
                }
                if candidate.is_usable(now) {
                    if selected_ids.insert(candidate.id.clone()) {
                        probe_ids.push(candidate.id.clone());
                        reserved_ids.push(candidate.id.clone());
                    }
                    selected_for_demand += 1;
                }
            }
            for candidate in &candidates {
                if selected_for_demand >= target_count {
                    break;
                }
                if selected_ids.contains(&candidate.id) || !candidate.matches(&demand) {
                    continue;
                }
                if candidate.needs_refresh(now) {
                    if selected_ids.insert(candidate.id.clone()) {
                        refresh_ids.push(candidate.id.clone());
                        probe_ids.push(candidate.id.clone());
                        reserved_ids.push(candidate.id.clone());
                    }
                    selected_for_demand += 1;
                }
            }
        }
        let reservation_id = if reserve_accounts {
            Some(Uuid::new_v4().to_string())
        } else {
            None
        };
        if let Some(reservation_id) = &reservation_id {
            reserve_accounts_for_redeem_tx(
                &mut tx,
                reservation_id,
                &reserved_ids,
                now as i64,
                reservation_cutoff,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(RedeemExportPreparation {
            estimated_account_count,
            refresh_account_ids: refresh_ids,
            probe_account_ids: probe_ids,
            reservation_id,
        })
    }

    pub async fn estimate_after_sale_export(
        &self,
        raw_codes: &[String],
    ) -> Result<RedeemAfterSalePreparation, DataError> {
        self.prepare_after_sale_export_inner(raw_codes, false).await
    }

    pub async fn prepare_after_sale_export(
        &self,
        raw_codes: &[String],
    ) -> Result<RedeemAfterSalePreparation, DataError> {
        self.prepare_after_sale_export_inner(raw_codes, true).await
    }

    async fn prepare_after_sale_export_inner(
        &self,
        raw_codes: &[String],
        reserve_accounts: bool,
    ) -> Result<RedeemAfterSalePreparation, DataError> {
        struct AfterSalePreparationCode {
            code_status: String,
            batch_status: String,
            redemption_id: Option<String>,
            accounts_per_code: usize,
            pool_id: String,
            plan_filter: Vec<String>,
            expires_at: Option<i64>,
            after_sale_limit: i64,
            after_sale_count: i64,
        }

        let now = unix_now_secs();
        let mut hashes = Vec::new();
        let mut seen_hashes = HashSet::new();
        for raw_code in raw_codes {
            let Some(normalized) = normalize_redeem_code(raw_code) else {
                continue;
            };
            let hash = redeem_code_hash(&normalized);
            if seen_hashes.insert(hash.clone()) {
                hashes.push(hash);
            }
        }

        let mut tx = if reserve_accounts {
            self.pool.begin_with("BEGIN IMMEDIATE").await?
        } else {
            self.pool.begin().await?
        };
        let reservation_cutoff = redeem_reservation_cutoff(now);
        if reserve_accounts {
            clear_expired_redeem_reservations_tx(&mut tx, reservation_cutoff).await?;
        }

        let mut code_rows = HashMap::new();
        for chunk in hashes.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                r#"
SELECT codes.code_hash, codes.status AS code_status, codes.redemption_id,
       batches.status AS batch_status, batches.accounts_per_code,
       batches.pool_id, batches.plan_filter_json, batches.expires_at, batches.after_sale_limit,
       COALESCE((
         SELECT COUNT(*)
         FROM redeem_after_sales AS after_sales
         WHERE after_sales.code_id = codes.id AND after_sales.status = 'success'
       ), 0) AS after_sale_count
FROM redeem_codes AS codes
JOIN redeem_code_batches AS batches ON batches.id = codes.batch_id
WHERE codes.code_hash IN (
"#,
            );
            {
                let mut separated = builder.separated(", ");
                for hash in chunk {
                    separated.push_bind(hash);
                }
                separated.push_unseparated(")");
            }
            let rows = builder.build().fetch_all(&mut *tx).await?;
            for row in rows {
                let hash: String = row.try_get("code_hash")?;
                let plan_filter = row
                    .try_get::<Option<String>, _>("plan_filter_json")?
                    .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                    .unwrap_or_default();
                code_rows.insert(
                    hash,
                    AfterSalePreparationCode {
                        code_status: row.try_get("code_status")?,
                        batch_status: row.try_get("batch_status")?,
                        redemption_id: row.try_get("redemption_id")?,
                        accounts_per_code: usize_from_i64(row.try_get("accounts_per_code")?),
                        pool_id: row.try_get("pool_id")?,
                        plan_filter,
                        expires_at: row.try_get("expires_at")?,
                        after_sale_limit: row.try_get("after_sale_limit")?,
                        after_sale_count: row.try_get("after_sale_count")?,
                    },
                );
            }
        }

        let redemption_ids = hashes
            .iter()
            .filter_map(|hash| {
                let row = code_rows.get(hash)?;
                if row.code_status == "disabled"
                    || row.batch_status != "active"
                    || row.expires_at.is_some_and(|value| value <= now as i64)
                    || row.after_sale_limit <= 0
                    || row.after_sale_count >= row.after_sale_limit
                {
                    return None;
                }
                row.redemption_id.clone()
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut redemption_account_ids = HashMap::new();
        for chunk in redemption_ids.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                "SELECT id, account_ids_json FROM redeem_redemptions WHERE id IN (",
            );
            {
                let mut separated = builder.separated(", ");
                for redemption_id in chunk {
                    separated.push_bind(redemption_id);
                }
                separated.push_unseparated(")");
            }
            let rows = builder.build().fetch_all(&mut *tx).await?;
            for row in rows {
                let redemption_id: String = row.try_get("id")?;
                let account_ids = serde_json::from_str::<Vec<String>>(
                    row.try_get::<String, _>("account_ids_json")?.as_str(),
                )
                .unwrap_or_default();
                redemption_account_ids.insert(redemption_id, account_ids);
            }
        }

        let mut demands = Vec::new();
        let mut seen_current_probe_ids = HashSet::new();
        let mut current_probe_ids = Vec::new();
        let mut estimated_account_count = 0_usize;
        for hash in hashes {
            let Some(row) = code_rows.get(&hash) else {
                continue;
            };
            if row.code_status == "disabled"
                || row.batch_status != "active"
                || row.expires_at.is_some_and(|value| value <= now as i64)
                || row.redemption_id.is_none()
                || row.after_sale_limit <= 0
                || row.after_sale_count >= row.after_sale_limit
            {
                continue;
            }

            let Some(redemption_id) = &row.redemption_id else {
                continue;
            };
            if let Some(account_ids) = redemption_account_ids.get(redemption_id) {
                for account_id in account_ids {
                    if seen_current_probe_ids.insert(account_id.clone()) {
                        current_probe_ids.push(account_id.clone());
                    }
                }
            }

            if row.accounts_per_code > 0 {
                estimated_account_count =
                    estimated_account_count.saturating_add(row.accounts_per_code);
                demands.push(RedeemAccountDemand {
                    count: row.accounts_per_code,
                    pool_id: row.pool_id.clone(),
                    plan_filter: row.plan_filter.clone(),
                });
            }
        }

        let (refresh_ids, replacement_probe_ids, reservation_id) = self
            .select_replacement_candidates_for_demands(
                &mut tx,
                demands,
                now,
                reserve_accounts,
                reservation_cutoff,
            )
            .await?;
        tx.commit().await?;
        Ok(RedeemAfterSalePreparation {
            estimated_account_count,
            refresh_account_ids: refresh_ids,
            current_probe_account_ids: current_probe_ids,
            replacement_probe_account_ids: replacement_probe_ids,
            reservation_id,
        })
    }

    async fn select_replacement_candidates_for_demands(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        demands: Vec<RedeemAccountDemand>,
        now: u64,
        reserve_accounts: bool,
        reservation_cutoff: i64,
    ) -> Result<(Vec<String>, Vec<String>, Option<String>), DataError> {
        if demands.is_empty() {
            return Ok((Vec::new(), Vec::new(), None));
        }
        let demand_pool_ids = demands
            .iter()
            .map(|demand| demand.pool_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT id, pool_id, plan_type, status, expires_at
FROM accounts
WHERE redeemed_at IS NULL AND status IN ('available', 'at_expired')
  AND (redeem_reservation_id IS NULL OR redeem_reserved_at IS NULL OR redeem_reserved_at <= 
"#,
        );
        builder.push_bind(reservation_cutoff);
        builder.push(")");
        builder.push(" AND pool_id IN (");
        {
            let mut separated = builder.separated(", ");
            for pool_id in &demand_pool_ids {
                separated.push_bind(pool_id);
            }
            separated.push_unseparated(")");
        }
        builder.push(" ORDER BY created_at ASC");
        let rows = builder.build().fetch_all(&mut **tx).await?;
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
        let mut reserved_ids = Vec::new();
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
                    if selected_ids.insert(candidate.id.clone()) {
                        probe_ids.push(candidate.id.clone());
                        reserved_ids.push(candidate.id.clone());
                    }
                    selected_for_demand += 1;
                }
            }
            for candidate in &candidates {
                if selected_for_demand >= target_count {
                    break;
                }
                if selected_ids.contains(&candidate.id) || !candidate.matches(&demand) {
                    continue;
                }
                if candidate.needs_refresh(now) {
                    if selected_ids.insert(candidate.id.clone()) {
                        refresh_ids.push(candidate.id.clone());
                        probe_ids.push(candidate.id.clone());
                        reserved_ids.push(candidate.id.clone());
                    }
                    selected_for_demand += 1;
                }
            }
        }
        let reservation_id = if reserve_accounts {
            Some(Uuid::new_v4().to_string())
        } else {
            None
        };
        if let Some(reservation_id) = &reservation_id {
            reserve_accounts_for_redeem_tx(
                tx,
                reservation_id,
                &reserved_ids,
                now as i64,
                reservation_cutoff,
            )
            .await?;
        }
        Ok((refresh_ids, probe_ids, reservation_id))
    }

    #[cfg(test)]
    pub async fn redeem_codes_for_export(
        &self,
        raw_codes: &[String],
        format: ExportFormat,
    ) -> Result<RedeemExportOutcome, DataError> {
        self.redeem_codes_for_export_with_verified_accounts(raw_codes, format, None)
            .await
    }

    #[cfg(test)]
    pub async fn redeem_codes_for_export_with_verified_accounts(
        &self,
        raw_codes: &[String],
        format: ExportFormat,
        verified_account_ids: Option<&[String]>,
    ) -> Result<RedeemExportOutcome, DataError> {
        if verified_account_ids.is_none() || raw_codes.len() > 1 {
            return self
                .redeem_codes_for_export_batch(raw_codes, format, verified_account_ids, None)
                .await;
        }
        let _redeem_guard = self.redemption_lock.lock().await;
        let mut successes = Vec::new();
        let mut failures = Vec::new();
        let mut all_auth_files = Vec::new();
        let mut all_account_ids = Vec::new();
        let now = unix_now_secs() as i64;
        let usable_after = now.saturating_add(ACCESS_TOKEN_REFRESH_GRACE_SECONDS as i64);
        let mut seen_hashes = HashSet::new();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

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
                if !plan_filter.is_empty() {
                    account_query.push(" AND lower(plan_type) IN (");
                    let mut separated = account_query.separated(", ");
                    for plan in &plan_filter {
                        separated.push_bind(plan.to_ascii_lowercase());
                    }
                    separated.push_unseparated(")");
                }
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
                account_query.push(" ORDER BY created_at ASC LIMIT ");
                account_query.push_bind(accounts_per_code);
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
                    let formatted_code = format_redeem_code(&normalized);
                    let verified_account_id_set = verified_account_ids.map(|ids| {
                        ids.iter()
                            .map(|value| value.trim().to_string())
                            .filter(|value| !value.is_empty())
                            .collect::<HashSet<_>>()
                    });
                    log_redeem_stock_shortage_tx(
                        &mut tx,
                        "single_export",
                        &formatted_code,
                        &pool_id,
                        &plan_filter,
                        usize::try_from(accounts_per_code).unwrap_or_default(),
                        account_ids.len(),
                        usable_after,
                        None,
                        verified_account_id_set.as_ref(),
                    )
                    .await;
                    failures.push(RedeemFailure {
                        code: formatted_code,
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

    pub async fn redeem_codes_for_export_with_prepared_accounts(
        &self,
        raw_codes: &[String],
        format: ExportFormat,
        reservation_id: Option<&str>,
        verified_account_ids: Option<&[String]>,
    ) -> Result<RedeemExportOutcome, DataError> {
        self.redeem_codes_for_export_batch(raw_codes, format, verified_account_ids, reservation_id)
            .await
    }

    async fn redeem_codes_for_export_batch(
        &self,
        raw_codes: &[String],
        format: ExportFormat,
        verified_account_ids: Option<&[String]>,
        reservation_id: Option<&str>,
    ) -> Result<RedeemExportOutcome, DataError> {
        struct CodeInput {
            code: String,
            hash: String,
        }

        struct CodeLookup {
            code_id: String,
            batch_id: String,
            code_status: String,
            batch_status: String,
            accounts_per_code: usize,
            pool_id: String,
            plan_filter: Vec<String>,
            expires_at: Option<i64>,
            redemption_id: Option<String>,
        }

        struct ExistingRedemption {
            code: String,
            redemption_id: String,
        }

        struct AccountDemand {
            code: String,
            code_id: String,
            batch_id: String,
            pool_id: String,
            plan_filter: Vec<String>,
            count: usize,
        }

        struct CandidateAccount {
            id: String,
            pool_id: String,
            plan_type: Option<String>,
        }

        impl CandidateAccount {
            fn matches(&self, demand: &AccountDemand) -> bool {
                self.pool_id == demand.pool_id
                    && (demand.plan_filter.is_empty()
                        || self.plan_type.as_ref().is_some_and(|value| {
                            demand
                                .plan_filter
                                .iter()
                                .any(|plan| plan.eq_ignore_ascii_case(value))
                        }))
            }
        }

        struct RedemptionInsert {
            id: String,
            code_id: String,
            batch_id: String,
            account_ids_json: String,
            export_snapshot_ciphertext: String,
        }

        struct AccountUpdate {
            account_id: String,
            code_id: String,
            redemption_id: String,
        }

        struct CodeUpdate {
            code_id: String,
            redemption_id: String,
        }

        let _redeem_guard = self.redemption_lock.lock().await;
        let verified_account_ids = verified_account_ids.map(|ids| {
            ids.iter()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect::<HashSet<_>>()
        });
        let reservation_id = reservation_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let now = unix_now_secs() as i64;
        let usable_after = now.saturating_add(ACCESS_TOKEN_REFRESH_GRACE_SECONDS as i64);
        let mut inputs = Vec::new();
        let mut failures = Vec::new();
        let mut seen_hashes = HashSet::new();
        for raw_code in raw_codes {
            let Some(normalized) = normalize_redeem_code(raw_code) else {
                failures.push(RedeemFailure {
                    code: raw_code.clone(),
                    reason: "兑换码格式无效".to_string(),
                });
                continue;
            };
            let hash = redeem_code_hash(&normalized);
            let code = format_redeem_code(&normalized);
            if !seen_hashes.insert(hash.clone()) {
                failures.push(RedeemFailure {
                    code,
                    reason: "兑换码重复提交".to_string(),
                });
                continue;
            }
            inputs.push(CodeInput { code, hash });
        }

        let mut successes = Vec::new();
        let mut all_auth_files = Vec::new();
        let mut all_account_ids = Vec::new();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let mut code_rows = HashMap::new();
        for chunk in inputs.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                r#"
SELECT codes.code_hash, codes.id AS code_id, codes.batch_id, codes.status AS code_status,
       codes.redemption_id, batches.status AS batch_status,
       batches.accounts_per_code, batches.pool_id, batches.plan_filter_json, batches.expires_at
FROM redeem_codes AS codes
JOIN redeem_code_batches AS batches ON batches.id = codes.batch_id
WHERE codes.code_hash IN (
"#,
            );
            {
                let mut separated = builder.separated(", ");
                for input in chunk {
                    separated.push_bind(&input.hash);
                }
                separated.push_unseparated(")");
            }
            let rows = builder.build().fetch_all(&mut *tx).await?;
            for row in rows {
                let hash: String = row.try_get("code_hash")?;
                let plan_filter = row
                    .try_get::<Option<String>, _>("plan_filter_json")?
                    .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                    .unwrap_or_default();
                code_rows.insert(
                    hash.clone(),
                    CodeLookup {
                        code_id: row.try_get("code_id")?,
                        batch_id: row.try_get("batch_id")?,
                        code_status: row.try_get("code_status")?,
                        batch_status: row.try_get("batch_status")?,
                        accounts_per_code: usize_from_i64(row.try_get("accounts_per_code")?),
                        pool_id: row.try_get("pool_id")?,
                        plan_filter,
                        expires_at: row.try_get("expires_at")?,
                        redemption_id: row.try_get("redemption_id")?,
                    },
                );
            }
        }

        let mut existing_redemptions = Vec::new();
        let mut demands = Vec::new();
        for input in &inputs {
            let Some(row) = code_rows.get(&input.hash) else {
                failures.push(RedeemFailure {
                    code: input.code.clone(),
                    reason: "兑换码不存在".to_string(),
                });
                continue;
            };
            if row.batch_status != "active" || row.code_status == "disabled" {
                failures.push(RedeemFailure {
                    code: input.code.clone(),
                    reason: "兑换码已停用".to_string(),
                });
                continue;
            }
            if row.expires_at.is_some_and(|value| value <= now) {
                failures.push(RedeemFailure {
                    code: input.code.clone(),
                    reason: "兑换码已过期".to_string(),
                });
                continue;
            }
            if let Some(redemption_id) = row.redemption_id.clone() {
                existing_redemptions.push(ExistingRedemption {
                    code: input.code.clone(),
                    redemption_id,
                });
                continue;
            }
            demands.push(AccountDemand {
                code: input.code.clone(),
                code_id: row.code_id.clone(),
                batch_id: row.batch_id.clone(),
                pool_id: row.pool_id.clone(),
                plan_filter: row.plan_filter.clone(),
                count: row.accounts_per_code,
            });
        }

        let mut redemption_snapshots = HashMap::new();
        let redemption_ids = existing_redemptions
            .iter()
            .map(|item| item.redemption_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        for chunk in redemption_ids.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                "SELECT id, account_ids_json, export_snapshot_ciphertext FROM redeem_redemptions WHERE id IN (",
            );
            {
                let mut separated = builder.separated(", ");
                for redemption_id in chunk {
                    separated.push_bind(redemption_id);
                }
                separated.push_unseparated(")");
            }
            let rows = builder.build().fetch_all(&mut *tx).await?;
            for row in rows {
                let redemption_id: String = row.try_get("id")?;
                let account_ids = serde_json::from_str::<Vec<String>>(
                    row.try_get::<String, _>("account_ids_json")?.as_str(),
                )
                .unwrap_or_default();
                let snapshot_ciphertext: String = row.try_get("export_snapshot_ciphertext")?;
                let auth_files = self
                    .secrets
                    .decrypt_json::<Vec<CodexAuthFile>>(&snapshot_ciphertext)?;
                redemption_snapshots.insert(redemption_id, (account_ids, auth_files));
            }
        }
        for existing in existing_redemptions {
            if let Some((account_ids, auth_files)) =
                redemption_snapshots.get(&existing.redemption_id)
            {
                successes.push(RedeemSuccess {
                    code: existing.code,
                    account_count: account_ids.len(),
                    after_sale_count: None,
                    replacement_account_count: None,
                });
                all_account_ids.extend(account_ids.clone());
                all_auth_files.extend(auth_files.clone());
            }
        }

        let mut allocations: Vec<Option<Vec<String>>> = vec![None; demands.len()];
        if !demands.is_empty() {
            let first_pool_id = demands[0].pool_id.clone();
            let first_plan_filter = normalized_plan_filter_key(&demands[0].plan_filter);
            let single_candidate_scope = demands.iter().all(|demand| {
                demand.pool_id == first_pool_id
                    && normalized_plan_filter_key(&demand.plan_filter) == first_plan_filter
            });
            let verified_scope_can_be_sql_filtered = match &verified_account_ids {
                None => true,
                Some(account_ids) => account_ids.len() <= 5_000,
            };
            let bounded_candidate_scope =
                single_candidate_scope && verified_scope_can_be_sql_filtered;
            let required_candidate_count = demands.iter().map(|demand| demand.count).sum::<usize>();
            let pool_ids = demands
                .iter()
                .map(|demand| demand.pool_id.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let mut builder = QueryBuilder::<Sqlite>::new(
                r#"
SELECT id, pool_id, plan_type
FROM accounts
WHERE redeemed_at IS NULL AND status = 'available'
  AND expires_at IS NOT NULL AND expires_at >
"#,
            );
            builder.push_bind(usable_after);
            if bounded_candidate_scope {
                builder.push(" AND pool_id = ");
                builder.push_bind(&first_pool_id);
                if !first_plan_filter.is_empty() {
                    builder.push(" AND lower(plan_type) IN (");
                    let mut separated = builder.separated(", ");
                    for plan in &first_plan_filter {
                        separated.push_bind(plan);
                    }
                    separated.push_unseparated(")");
                }
            } else {
                builder.push(" AND pool_id IN (");
                {
                    let mut separated = builder.separated(", ");
                    for pool_id in &pool_ids {
                        separated.push_bind(pool_id);
                    }
                    separated.push_unseparated(")");
                }
            }
            if let Some(reservation_id) = &reservation_id {
                builder.push(" AND redeem_reservation_id = ");
                builder.push_bind(reservation_id);
            } else if let Some(verified_account_ids) = &verified_account_ids {
                if verified_account_ids.is_empty() {
                    builder.push(" AND 1 = 0");
                } else if verified_account_ids.len() <= 5_000 {
                    // Keep SQLite bind counts bounded; larger verified sets are filtered in memory below.
                    builder.push(" AND id IN (");
                    let mut separated = builder.separated(", ");
                    for account_id in verified_account_ids {
                        separated.push_bind(account_id);
                    }
                    separated.push_unseparated(")");
                }
            }
            builder.push(" ORDER BY created_at ASC");
            if bounded_candidate_scope {
                builder.push(" LIMIT ");
                builder.push_bind(required_candidate_count as i64);
            }
            let candidates = builder
                .build()
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .map(|row| {
                    Ok(CandidateAccount {
                        id: row.try_get("id")?,
                        pool_id: row.try_get("pool_id")?,
                        plan_type: row.try_get("plan_type")?,
                    })
                })
                .collect::<Result<Vec<_>, DataError>>()?;
            let mut selected_ids = HashSet::new();
            let mut shortage_log_keys = HashSet::new();
            for (index, demand) in demands.iter().enumerate() {
                let mut account_ids = Vec::with_capacity(demand.count);
                for candidate in &candidates {
                    if account_ids.len() >= demand.count {
                        break;
                    }
                    if selected_ids.contains(&candidate.id)
                        || !candidate.matches(demand)
                        || verified_account_ids
                            .as_ref()
                            .is_some_and(|ids| !ids.contains(&candidate.id))
                    {
                        continue;
                    }
                    account_ids.push(candidate.id.clone());
                }
                if account_ids.len() < demand.count {
                    let shortage_log_key = redeem_stock_shortage_log_key(
                        &demand.pool_id,
                        &demand.plan_filter,
                        reservation_id.as_deref(),
                        verified_account_ids.as_ref(),
                    );
                    if shortage_log_keys.insert(shortage_log_key) {
                        log_redeem_stock_shortage_tx(
                            &mut tx,
                            "batch_export",
                            &demand.code,
                            &demand.pool_id,
                            &demand.plan_filter,
                            demand.count,
                            account_ids.len(),
                            usable_after,
                            reservation_id.as_deref(),
                            verified_account_ids.as_ref(),
                        )
                        .await;
                    }
                    failures.push(RedeemFailure {
                        code: demand.code.clone(),
                        reason: "可兑换账号库存不足".to_string(),
                    });
                    continue;
                }
                selected_ids.extend(account_ids.iter().cloned());
                allocations[index] = Some(account_ids);
            }
        }

        let selected_account_ids = allocations
            .iter()
            .flatten()
            .flat_map(|account_ids| account_ids.iter().cloned())
            .collect::<Vec<_>>();
        let auth_files = self
            .load_auth_files_for_ids_tx(&mut tx, &selected_account_ids)
            .await?
            .into_iter()
            .map(|(summary, auth)| (summary.id, auth.normalized()))
            .collect::<HashMap<_, _>>();

        let mut redemptions = Vec::new();
        let mut account_updates = Vec::new();
        let mut code_updates = Vec::new();
        let mut batch_counts: HashMap<String, i64> = HashMap::new();
        for (index, demand) in demands.into_iter().enumerate() {
            let Some(account_ids) = allocations[index].clone() else {
                continue;
            };
            let auth_snapshot = account_ids
                .iter()
                .filter_map(|account_id| auth_files.get(account_id).cloned())
                .collect::<Vec<_>>();
            if auth_snapshot.len() != account_ids.len() {
                return Err(DataError::NotFound);
            }
            let redemption_id = Uuid::new_v4().to_string();
            let snapshot_ciphertext = self.secrets.encrypt_json(&auth_snapshot)?;
            redemptions.push(RedemptionInsert {
                id: redemption_id.clone(),
                code_id: demand.code_id.clone(),
                batch_id: demand.batch_id.clone(),
                account_ids_json: serde_json::to_string(&account_ids)
                    .unwrap_or_else(|_| "[]".to_string()),
                export_snapshot_ciphertext: snapshot_ciphertext,
            });
            for account_id in &account_ids {
                account_updates.push(AccountUpdate {
                    account_id: account_id.clone(),
                    code_id: demand.code_id.clone(),
                    redemption_id: redemption_id.clone(),
                });
            }
            code_updates.push(CodeUpdate {
                code_id: demand.code_id,
                redemption_id,
            });
            *batch_counts.entry(demand.batch_id).or_default() += 1;
            successes.push(RedeemSuccess {
                code: demand.code,
                account_count: account_ids.len(),
                after_sale_count: None,
                replacement_account_count: None,
            });
            all_account_ids.extend(account_ids);
            all_auth_files.extend(auth_snapshot);
        }

        for chunk in redemptions.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                r#"
INSERT INTO redeem_redemptions (
  id, code_id, batch_id, export_format, account_ids_json, export_snapshot_ciphertext, created_at
)
"#,
            );
            builder.push_values(chunk, |mut row, redemption| {
                row.push_bind(&redemption.id)
                    .push_bind(&redemption.code_id)
                    .push_bind(&redemption.batch_id)
                    .push_bind(format.as_str())
                    .push_bind(&redemption.account_ids_json)
                    .push_bind(&redemption.export_snapshot_ciphertext)
                    .push_bind(now);
            });
            builder.build().execute(&mut *tx).await?;
        }

        for chunk in account_updates.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new("UPDATE accounts SET redeemed_at = ");
            builder
                .push_bind(now)
                .push(", updated_at = ")
                .push_bind(now)
                .push(", redeem_code_id = CASE id ");
            for update in chunk {
                builder
                    .push(" WHEN ")
                    .push_bind(&update.account_id)
                    .push(" THEN ")
                    .push_bind(&update.code_id);
            }
            builder.push(" END, redemption_id = CASE id ");
            for update in chunk {
                builder
                    .push(" WHEN ")
                    .push_bind(&update.account_id)
                    .push(" THEN ")
                    .push_bind(&update.redemption_id);
            }
            builder.push(" END WHERE redeemed_at IS NULL AND id IN (");
            {
                let mut separated = builder.separated(", ");
                for update in chunk {
                    separated.push_bind(&update.account_id);
                }
                separated.push_unseparated(")");
            }
            let updated = builder.build().execute(&mut *tx).await?;
            if updated.rows_affected() != chunk.len() as u64 {
                return Err(DataError::NotFound);
            }
        }

        for chunk in code_updates.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                "UPDATE redeem_codes SET status = 'redeemed', redeemed_at = ",
            );
            builder
                .push_bind(now)
                .push(", updated_at = ")
                .push_bind(now)
                .push(", redemption_id = CASE id ");
            for update in chunk {
                builder
                    .push(" WHEN ")
                    .push_bind(&update.code_id)
                    .push(" THEN ")
                    .push_bind(&update.redemption_id);
            }
            builder.push(" END WHERE id IN (");
            {
                let mut separated = builder.separated(", ");
                for update in chunk {
                    separated.push_bind(&update.code_id);
                }
                separated.push_unseparated(")");
            }
            let updated = builder.build().execute(&mut *tx).await?;
            if updated.rows_affected() != chunk.len() as u64 {
                return Err(DataError::NotFound);
            }
        }

        for (batch_id, count) in batch_counts {
            sqlx::query(
                "UPDATE redeem_code_batches SET redeemed_count = redeemed_count + ?, updated_at = ? WHERE id = ?",
            )
            .bind(count)
            .bind(now)
            .bind(batch_id)
            .execute(&mut *tx)
            .await?;
        }

        if let Some(reservation_id) = &reservation_id {
            clear_redeem_reservation_tx(&mut tx, reservation_id, now).await?;
        }

        let document = export_accounts(format, &all_auth_files);
        sqlx::query(
            "INSERT INTO account_exports (id, format, source, account_ids_json, account_count, created_at) VALUES (?, ?, 'redeem', ?, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(format.as_str())
        .bind(json!(all_account_ids).to_string())
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

    #[cfg(test)]
    pub async fn redeem_after_sale_for_export_with_verified_accounts(
        &self,
        raw_codes: &[String],
        format: ExportFormat,
        verified_current_account_ids: Option<&[String]>,
    ) -> Result<RedeemExportOutcome, DataError> {
        self.redeem_after_sale_for_export_with_prepared_accounts(
            raw_codes,
            format,
            verified_current_account_ids,
            None,
            verified_current_account_ids,
        )
        .await
    }

    pub async fn redeem_after_sale_for_export_with_prepared_accounts(
        &self,
        raw_codes: &[String],
        format: ExportFormat,
        verified_current_account_ids: Option<&[String]>,
        reservation_id: Option<&str>,
        verified_replacement_account_ids: Option<&[String]>,
    ) -> Result<RedeemExportOutcome, DataError> {
        let _redeem_guard = self.redemption_lock.lock().await;
        let verified_current_account_ids = verified_current_account_ids.map(|ids| {
            ids.iter()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect::<HashSet<_>>()
        });
        let verified_replacement_account_ids = verified_replacement_account_ids.map(|ids| {
            ids.iter()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect::<HashSet<_>>()
        });
        let reservation_id = reservation_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let mut successes = Vec::new();
        let mut failures = Vec::new();
        let mut all_auth_files = Vec::new();
        let mut all_account_ids = Vec::new();
        let now = unix_now_secs() as i64;
        let usable_after = now.saturating_add(ACCESS_TOKEN_REFRESH_GRACE_SECONDS as i64);
        let mut seen_hashes = HashSet::new();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut shortage_log_keys = HashSet::new();

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
            if !plan_filter.is_empty() {
                account_query.push(" AND lower(plan_type) IN (");
                let mut separated = account_query.separated(", ");
                for plan in &plan_filter {
                    separated.push_bind(plan.to_ascii_lowercase());
                }
                separated.push_unseparated(")");
            }
            if let Some(reservation_id) = &reservation_id {
                account_query.push(" AND redeem_reservation_id = ");
                account_query.push_bind(reservation_id);
            }
            if let Some(verified_ids) = &verified_replacement_account_ids {
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
            account_query.push(" ORDER BY created_at ASC LIMIT ");
            account_query.push_bind(accounts_per_code.max(0));
            let rows = account_query.build().fetch_all(&mut *tx).await?;
            let new_account_ids = rows
                .into_iter()
                .filter_map(|row| {
                    let id: String = row.try_get("id").ok()?;
                    let plan_type: Option<String> = row.try_get("plan_type").ok();
                    if verified_replacement_account_ids
                        .as_ref()
                        .is_some_and(|ids| !ids.contains(&id))
                    {
                        return None;
                    }
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
                let shortage_log_key = redeem_stock_shortage_log_key(
                    &pool_id,
                    &plan_filter,
                    reservation_id.as_deref(),
                    verified_replacement_account_ids.as_ref(),
                );
                if shortage_log_keys.insert(shortage_log_key) {
                    log_redeem_stock_shortage_tx(
                        &mut tx,
                        "after_sale_export",
                        &formatted_code,
                        &pool_id,
                        &plan_filter,
                        required_count,
                        new_account_ids.len(),
                        usable_after,
                        reservation_id.as_deref(),
                        verified_replacement_account_ids.as_ref(),
                    )
                    .await;
                }
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

        if let Some(reservation_id) = &reservation_id {
            clear_redeem_reservation_tx(&mut tx, reservation_id, now).await?;
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
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut unique_ids = Vec::new();
        let mut seen_ids = HashSet::new();
        for id in ids {
            if seen_ids.insert(id.clone()) {
                unique_ids.push(id.clone());
            }
        }

        let mut pairs = HashMap::new();
        for chunk in unique_ids.chunks(500) {
            let mut builder = QueryBuilder::<Sqlite>::new(
                r#"
SELECT a.id, a.pool_id, p.name AS pool_name, a.email, a.name, a.account_id, a.plan_type, a.status, a.access_token_preview,
       a.refresh_token_preview, a.expires_at, a.last_refresh_at, a.last_probe_at,
       a.quota_snapshot, a.redeem_code_id, rc.masked_code AS redeem_code_masked, a.redemption_id,
       a.redeemed_at, a.created_at, a.updated_at, a.auth_file_ciphertext
FROM accounts a
LEFT JOIN account_pools p ON p.id = a.pool_id
LEFT JOIN redeem_codes rc ON rc.id = a.redeem_code_id
WHERE a.id IN (
"#,
            );
            {
                let mut separated = builder.separated(", ");
                for id in chunk {
                    separated.push_bind(id);
                }
                separated.push_unseparated(")");
            }
            let rows = builder.build().fetch_all(&mut **tx).await?;
            for row in rows {
                let summary = account_summary_from_row(&row)?;
                let ciphertext: String = row.try_get("auth_file_ciphertext")?;
                pairs.insert(
                    summary.id.clone(),
                    (
                        summary,
                        self.secrets.decrypt_json::<CodexAuthFile>(&ciphertext)?,
                    ),
                );
            }
        }

        let mut out = Vec::new();
        for id in ids {
            let Some((summary, auth_file)) = pairs.get(id) else {
                return Err(DataError::NotFound);
            };
            out.push((summary.clone(), auth_file.clone()));
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

fn redeem_reservation_cutoff(now: u64) -> i64 {
    now.saturating_sub(REDEEM_RESERVATION_TTL_SECONDS) as i64
}

async fn clear_expired_redeem_reservations_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    reservation_cutoff: i64,
) -> Result<(), DataError> {
    sqlx::query(
        r#"
UPDATE accounts
SET redeem_reservation_id = NULL, redeem_reserved_at = NULL
WHERE redeem_reserved_at IS NOT NULL AND redeem_reserved_at <= ?
"#,
    )
    .bind(reservation_cutoff)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn reserve_accounts_for_redeem_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    reservation_id: &str,
    account_ids: &[String],
    now: i64,
    reservation_cutoff: i64,
) -> Result<(), DataError> {
    if account_ids.is_empty() {
        return Ok(());
    }
    for chunk in account_ids.chunks(500) {
        let mut builder =
            QueryBuilder::<Sqlite>::new("UPDATE accounts SET redeem_reservation_id = ");
        builder
            .push_bind(reservation_id)
            .push(", redeem_reserved_at = ")
            .push_bind(now)
            .push(", updated_at = ")
            .push_bind(now)
            .push(" WHERE redeemed_at IS NULL AND id IN (");
        {
            let mut separated = builder.separated(", ");
            for account_id in chunk {
                separated.push_bind(account_id);
            }
            separated.push_unseparated(")");
        }
        builder.push(
            " AND (redeem_reservation_id IS NULL OR redeem_reserved_at IS NULL OR redeem_reserved_at <= ",
        );
        builder.push_bind(reservation_cutoff).push(")");
        let updated = builder.build().execute(&mut **tx).await?;
        if updated.rows_affected() != chunk.len() as u64 {
            return Err(DataError::NotFound);
        }
    }
    Ok(())
}

struct RedeemStockShortageSnapshot {
    unredeemed_in_pool: i64,
    available_in_pool: i64,
    redeemable_in_pool: i64,
    available_without_expires_at: i64,
    available_expired_or_in_grace: i64,
    matching_redeemable: i64,
    scoped_redeemable: i64,
}

fn redeem_stock_shortage_log_key(
    pool_id: &str,
    plan_filter: &[String],
    reservation_id: Option<&str>,
    verified_account_ids: Option<&HashSet<String>>,
) -> String {
    let normalized_plans = normalized_plan_filter_key(plan_filter);
    format!(
        "{}|{}|{}|{}",
        pool_id,
        normalized_plans.join(","),
        reservation_id.unwrap_or_default(),
        verified_account_ids
            .map(|account_ids| account_ids.len())
            .unwrap_or_default()
    )
}

fn normalized_plan_filter_key(plan_filter: &[String]) -> Vec<String> {
    let mut normalized_plans = plan_filter
        .iter()
        .map(|plan| plan.trim().to_ascii_lowercase())
        .filter(|plan| !plan.is_empty())
        .collect::<Vec<_>>();
    normalized_plans.sort();
    normalized_plans.dedup();
    normalized_plans
}

#[allow(clippy::too_many_arguments)]
async fn log_redeem_stock_shortage_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    context: &str,
    code: &str,
    pool_id: &str,
    plan_filter: &[String],
    required_count: usize,
    selected_count: usize,
    usable_after: i64,
    reservation_id: Option<&str>,
    verified_account_ids: Option<&HashSet<String>>,
) {
    let snapshot = match load_redeem_stock_shortage_snapshot_tx(
        tx,
        pool_id,
        plan_filter,
        usable_after,
        reservation_id,
        verified_account_ids,
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                context,
                code = %mask_redeem_code(code),
                pool_id,
                plan_filter = ?plan_filter,
                required_count,
                selected_count,
                usable_after,
                reservation_id = %reservation_id.unwrap_or(""),
                has_reservation = reservation_id.is_some(),
                verified_scope_count = verified_account_ids
                    .map(|account_ids| account_ids.len())
                    .unwrap_or_default(),
                error = %error,
                "redeem stock shortage; failed to load stock snapshot"
            );
            return;
        }
    };
    let verified_scope_count = verified_account_ids
        .map(|account_ids| account_ids.len())
        .unwrap_or_default();
    tracing::warn!(
        context,
        code = %mask_redeem_code(code),
        pool_id,
        plan_filter = ?plan_filter,
        required_count,
        selected_count,
        usable_after,
        reservation_id = %reservation_id.unwrap_or(""),
        has_reservation = reservation_id.is_some(),
        verified_scope_count,
        unredeemed_in_pool = snapshot.unredeemed_in_pool,
        available_in_pool = snapshot.available_in_pool,
        redeemable_in_pool = snapshot.redeemable_in_pool,
        available_without_expires_at = snapshot.available_without_expires_at,
        available_expired_or_in_grace = snapshot.available_expired_or_in_grace,
        matching_redeemable = snapshot.matching_redeemable,
        scoped_redeemable = snapshot.scoped_redeemable,
        "redeem stock shortage"
    );
}

async fn load_redeem_stock_shortage_snapshot_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    pool_id: &str,
    plan_filter: &[String],
    usable_after: i64,
    reservation_id: Option<&str>,
    verified_account_ids: Option<&HashSet<String>>,
) -> Result<RedeemStockShortageSnapshot, DataError> {
    let row = sqlx::query(
        r#"
SELECT
  COUNT(*) AS unredeemed_in_pool,
  COALESCE(SUM(CASE WHEN status = 'available' THEN 1 ELSE 0 END), 0) AS available_in_pool,
  COALESCE(SUM(CASE WHEN status = 'available' AND expires_at IS NOT NULL AND expires_at > ? THEN 1 ELSE 0 END), 0) AS redeemable_in_pool,
  COALESCE(SUM(CASE WHEN status = 'available' AND expires_at IS NULL THEN 1 ELSE 0 END), 0) AS available_without_expires_at,
  COALESCE(SUM(CASE WHEN status = 'available' AND expires_at IS NOT NULL AND expires_at <= ? THEN 1 ELSE 0 END), 0) AS available_expired_or_in_grace
FROM accounts
WHERE pool_id = ? AND redeemed_at IS NULL
"#,
    )
    .bind(usable_after)
    .bind(usable_after)
    .bind(pool_id)
    .fetch_one(&mut **tx)
    .await?;
    let matching_redeemable =
        count_redeemable_accounts_tx(tx, pool_id, plan_filter, usable_after, None, None).await?;
    let scoped_redeemable = if let Some(account_ids) = verified_account_ids {
        if account_ids.is_empty() {
            0
        } else {
            let account_ids = account_ids.iter().collect::<Vec<_>>();
            let mut total = 0_i64;
            for chunk in account_ids.chunks(500) {
                total += count_redeemable_accounts_tx(
                    tx,
                    pool_id,
                    plan_filter,
                    usable_after,
                    reservation_id,
                    Some(chunk),
                )
                .await?;
            }
            total
        }
    } else {
        count_redeemable_accounts_tx(tx, pool_id, plan_filter, usable_after, reservation_id, None)
            .await?
    };
    Ok(RedeemStockShortageSnapshot {
        unredeemed_in_pool: row.try_get("unredeemed_in_pool")?,
        available_in_pool: row.try_get("available_in_pool")?,
        redeemable_in_pool: row.try_get("redeemable_in_pool")?,
        available_without_expires_at: row.try_get("available_without_expires_at")?,
        available_expired_or_in_grace: row.try_get("available_expired_or_in_grace")?,
        matching_redeemable,
        scoped_redeemable,
    })
}

async fn count_redeemable_accounts_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    pool_id: &str,
    plan_filter: &[String],
    usable_after: i64,
    reservation_id: Option<&str>,
    account_ids: Option<&[&String]>,
) -> Result<i64, DataError> {
    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT COUNT(*) AS count FROM accounts WHERE redeemed_at IS NULL AND status = 'available' AND pool_id = ",
    );
    builder.push_bind(pool_id);
    builder.push(" AND expires_at IS NOT NULL AND expires_at > ");
    builder.push_bind(usable_after);
    if !plan_filter.is_empty() {
        builder.push(" AND lower(plan_type) IN (");
        let mut separated = builder.separated(", ");
        for plan in plan_filter {
            separated.push_bind(plan.to_ascii_lowercase());
        }
        separated.push_unseparated(")");
    }
    if let Some(reservation_id) = reservation_id {
        builder.push(" AND redeem_reservation_id = ");
        builder.push_bind(reservation_id);
    }
    if let Some(account_ids) = account_ids {
        if account_ids.is_empty() {
            builder.push(" AND 1 = 0");
        } else {
            builder.push(" AND id IN (");
            let mut separated = builder.separated(", ");
            for account_id in account_ids {
                separated.push_bind((*account_id).as_str());
            }
            separated.push_unseparated(")");
        }
    }
    let row = builder.build().fetch_one(&mut **tx).await?;
    Ok(row.try_get("count")?)
}

async fn clear_redeem_reservation_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    reservation_id: &str,
    now: i64,
) -> Result<(), DataError> {
    sqlx::query(
        r#"
UPDATE accounts
SET redeem_reservation_id = NULL, redeem_reserved_at = NULL, updated_at = ?
WHERE redeem_reservation_id = ?
"#,
    )
    .bind(now)
    .bind(reservation_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
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
    ensure_sqlite_column(
        pool,
        "accounts",
        "redeem_reservation_id",
        "ALTER TABLE accounts ADD COLUMN redeem_reservation_id TEXT",
    )
    .await?;
    ensure_sqlite_column(
        pool,
        "accounts",
        "redeem_reserved_at",
        "ALTER TABLE accounts ADD COLUMN redeem_reserved_at INTEGER",
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
        "CREATE INDEX IF NOT EXISTS idx_accounts_pool_available ON accounts(pool_id, status, redeemed_at, expires_at, created_at)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_accounts_unredeemed_created ON accounts(redeemed_at, created_at)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_accounts_redeem_reservation ON accounts(redeem_reservation_id, redeem_reserved_at)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
CREATE INDEX IF NOT EXISTS idx_accounts_pool_reservation_candidates
ON accounts(pool_id, status, redeemed_at, redeem_reserved_at, created_at)
WHERE redeemed_at IS NULL AND status IN ('available', 'at_expired')
"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_accounts_reservation_available ON accounts(redeem_reservation_id, status, redeemed_at, expires_at, pool_id, created_at)",
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

fn env_u32_clamped(name: &str, fallback: u32, min: u32, max: u32) -> u32 {
    let Some(raw) = std::env::var(name).ok().filter(|value| !value.is_empty()) else {
        return fallback;
    };
    raw.parse::<u32>()
        .map(|value| value.clamp(min, max))
        .unwrap_or(fallback)
}

fn env_u64_clamped(name: &str, fallback: u64, min: u64, max: u64) -> u64 {
    let Some(raw) = std::env::var(name).ok().filter(|value| !value.is_empty()) else {
        return fallback;
    };
    raw.parse::<u64>()
        .map(|value| value.clamp(min, max))
        .unwrap_or(fallback)
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportAccountResult {
    pub source_index: usize,
    pub id: Option<String>,
    pub email: Option<String>,
    pub external_account_id: Option<String>,
    pub pool_id: String,
    pub action: String,
    pub status: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportAccountsOutcome {
    pub imported: usize,
    pub updated: usize,
    pub results: Vec<ImportAccountResult>,
}

struct PreparedImportAccount {
    source_index: usize,
    auth_file: CodexAuthFile,
    fingerprint: String,
    legacy_fingerprint: String,
    email_key: Option<String>,
    ciphertext: String,
    expires_at: Option<i64>,
    status: String,
    external_account_id: Option<String>,
}

async fn load_import_ids_by_fingerprint_tx(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    fingerprints: &[String],
) -> Result<HashMap<String, String>, DataError> {
    let mut out = HashMap::new();
    let unique = fingerprints
        .iter()
        .filter(|value| !value.is_empty())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    for chunk in unique.chunks(500) {
        if chunk.is_empty() {
            continue;
        }
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT auth_fingerprint, id FROM accounts WHERE ");
        builder.push("auth_fingerprint IN (");
        let mut separated = builder.separated(", ");
        for fingerprint in chunk {
            separated.push_bind(fingerprint);
        }
        separated.push_unseparated(")");
        let rows = builder.build().fetch_all(&mut **tx).await?;
        for row in rows {
            let fingerprint: String = row.try_get("auth_fingerprint")?;
            let id: String = row.try_get("id")?;
            out.entry(fingerprint).or_insert(id);
        }
    }
    Ok(out)
}

async fn load_import_ids_by_legacy_tx(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    keys: &[(String, String)],
) -> Result<HashMap<(String, String), String>, DataError> {
    let mut out = HashMap::new();
    let unique = keys
        .iter()
        .filter(|(fingerprint, email)| !fingerprint.is_empty() && !email.is_empty())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    for chunk in unique.chunks(100) {
        if chunk.is_empty() {
            continue;
        }
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT auth_fingerprint, lower(email) AS email_key, id FROM accounts WHERE (auth_fingerprint, lower(email)) IN (",
        );
        for (index, (fingerprint, email)) in chunk.iter().enumerate() {
            if index > 0 {
                builder.push(", ");
            }
            builder.push("(");
            builder.push_bind(fingerprint);
            builder.push(", ");
            builder.push_bind(email);
            builder.push(")");
        }
        builder.push(")");
        let rows = builder.build().fetch_all(&mut **tx).await?;
        for row in rows {
            let fingerprint: String = row.try_get("auth_fingerprint")?;
            let email: String = row.try_get("email_key")?;
            let id: String = row.try_get("id")?;
            out.entry((fingerprint, email)).or_insert(id);
        }
    }
    Ok(out)
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
    pub stats: AccountPoolStats,
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
    pub pool_ids: Option<String>,
    pub search: Option<String>,
    pub status: Option<String>,
    pub statuses: Option<String>,
    pub redeemed: Option<bool>,
    pub redeemed_values: Option<String>,
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
    let statuses = csv_filter_values(query.statuses.as_deref());
    if !statuses.is_empty() {
        push_and(builder);
        builder.push("a.status IN (");
        {
            let mut separated = builder.separated(", ");
            for status in statuses {
                separated.push_bind(status);
            }
            separated.push_unseparated(")");
        }
    } else if let Some(status) = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        push_and(builder);
        builder.push("a.status = ").push_bind(status.to_string());
    }

    let redeemed_values = redeemed_filter_values(query.redeemed_values.as_deref());
    if redeemed_values.len() == 1 {
        push_and(builder);
        if redeemed_values[0] {
            builder.push("a.redeemed_at IS NOT NULL");
        } else {
            builder.push("a.redeemed_at IS NULL");
        }
    } else if let Some(redeemed) = query.redeemed {
        push_and(builder);
        if redeemed {
            builder.push("a.redeemed_at IS NOT NULL");
        } else {
            builder.push("a.redeemed_at IS NULL");
        }
    }

    let pool_ids = csv_filter_values(query.pool_ids.as_deref())
        .into_iter()
        .filter_map(|value| normalize_optional_pool_id(Some(&value)))
        .collect::<Vec<_>>();
    if !pool_ids.is_empty() {
        push_and(builder);
        builder.push("a.pool_id IN (");
        {
            let mut separated = builder.separated(", ");
            for pool_id in pool_ids {
                separated.push_bind(pool_id);
            }
            separated.push_unseparated(")");
        }
    } else if let Some(pool_id) = normalize_optional_pool_id(query.pool_id.as_deref()) {
        push_and(builder);
        builder.push("a.pool_id = ").push_bind(pool_id);
    }
}

fn csv_filter_values(raw: Option<&str>) -> Vec<String> {
    let mut seen = HashSet::new();
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn redeemed_filter_values(raw: Option<&str>) -> Vec<bool> {
    let mut values = Vec::new();
    for value in csv_filter_values(raw) {
        let parsed = match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "redeemed" => Some(true),
            "false" | "0" | "unredeemed" => Some(false),
            _ => None,
        };
        if let Some(parsed) = parsed {
            if !values.contains(&parsed) {
                values.push(parsed);
            }
        }
    }
    values
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
        self.status != AccountStatus::Available.as_str()
            || self.expires_at.is_none_or(|expires_at| {
                access_token_needs_refresh(
                    Some(expires_at),
                    now,
                    ACCESS_TOKEN_REFRESH_GRACE_SECONDS,
                )
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
        stats: AccountPoolStats::default(),
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
pub struct DeleteRedeemBatchOutcome {
    pub deleted: bool,
    pub accounts_reset: usize,
    pub codes_deleted: usize,
    pub redemptions_deleted: usize,
    pub after_sales_deleted: usize,
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
    pub reservation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemAfterSalePreparation {
    pub estimated_account_count: usize,
    pub refresh_account_ids: Vec<String>,
    pub current_probe_account_ids: Vec<String>,
    pub replacement_probe_account_ids: Vec<String>,
    pub reservation_id: Option<String>,
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

    fn temp_database_url() -> String {
        let path =
            std::env::temp_dir().join(format!("aether-pool-test-{}.sqlite3", Uuid::new_v4()));
        format!("sqlite://{}", path.display())
    }

    async fn temp_repo() -> AccountPoolRepository {
        AccountPoolRepository::connect(&temp_database_url(), "test-secret")
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

    fn parsed_expiring_soon_account(account_id: &str, access_token: &str) -> ParsedAccount {
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
                expires_at: Some(json!(
                    unix_now_secs().saturating_add(ACCESS_TOKEN_REFRESH_GRACE_SECONDS / 2)
                )),
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

    fn document_access_tokens(value: &Value) -> Vec<String> {
        if let Some(token) = value.get("access_token").and_then(Value::as_str) {
            return vec![token.to_string()];
        }
        if let Some(accounts) = value.as_array() {
            return accounts
                .iter()
                .filter_map(|account| account.get("access_token").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect();
        }
        value
            .get("accounts")
            .and_then(Value::as_array)
            .map(|accounts| {
                accounts
                    .iter()
                    .filter_map(|account| {
                        account
                            .get("credentials")
                            .and_then(|credentials| credentials.get("access_token"))
                            .and_then(Value::as_str)
                    })
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn stress_env_usize(name: &str, fallback: usize, min: usize, max: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .map(|value| value.clamp(min, max))
            .unwrap_or(fallback)
    }

    fn percentile_ms(durations: &[Duration], percentile: usize) -> f64 {
        if durations.is_empty() {
            return 0.0;
        }
        let mut micros = durations
            .iter()
            .map(Duration::as_micros)
            .collect::<Vec<_>>();
        micros.sort_unstable();
        let index = ((micros.len() - 1) * percentile.min(100) + 50) / 100;
        micros[index] as f64 / 1000.0
    }

    #[tokio::test]
    async fn import_accounts_returns_per_item_results_for_imports_and_updates() {
        let repo = temp_repo().await;
        let outcome = repo
            .import_accounts(&[
                parsed_account("result-1", "access-1"),
                parsed_expired_account("result-2", "access-2"),
            ])
            .await
            .unwrap();

        assert_eq!(outcome.imported, 2);
        assert_eq!(outcome.updated, 0);
        assert_eq!(outcome.results.len(), 2);
        assert_eq!(outcome.results[0].source_index, 0);
        assert!(outcome.results[0].id.is_some());
        assert_eq!(
            outcome.results[0].email.as_deref(),
            Some("result-1@example.com")
        );
        assert_eq!(
            outcome.results[0].external_account_id.as_deref(),
            Some("result-1")
        );
        assert_eq!(outcome.results[0].pool_id, DEFAULT_ACCOUNT_POOL_ID);
        assert_eq!(outcome.results[0].action, "imported");
        assert_eq!(
            outcome.results[0].status.as_deref(),
            Some(AccountStatus::Available.as_str())
        );
        assert_eq!(outcome.results[1].source_index, 1);
        assert_eq!(outcome.results[1].action, "imported");
        assert_eq!(
            outcome.results[1].status.as_deref(),
            Some(AccountStatus::AtExpired.as_str())
        );
        assert!(outcome.results.iter().all(|result| result.error.is_none()));

        let first_id = outcome.results[0].id.clone().unwrap();
        let update = repo
            .import_accounts(&[parsed_account("result-1", "access-1-new")])
            .await
            .unwrap();
        assert_eq!(update.imported, 0);
        assert_eq!(update.updated, 1);
        assert_eq!(update.results.len(), 1);
        assert_eq!(update.results[0].source_index, 0);
        assert_eq!(update.results[0].id.as_deref(), Some(first_id.as_str()));
        assert_eq!(update.results[0].action, "updated");
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

        let pools = repo.list_account_pools().await.unwrap();
        let left_pool = pools.iter().find(|pool| pool.id == left.id).unwrap();
        let right_pool = pools.iter().find(|pool| pool.id == right.id).unwrap();
        assert_eq!(left_pool.stats.total, 1);
        assert_eq!(left_pool.stats.available, 1);
        assert_eq!(right_pool.stats.total, 1);
        assert_eq!(right_pool.stats.available, 1);

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
            .redeem_after_sale_for_export_with_prepared_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                Some(&prep.current_probe_account_ids),
                prep.reservation_id.as_deref(),
                Some(&prep.replacement_probe_account_ids),
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
    async fn account_stats_available_matches_redeemable_stock() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_expiring_soon_account("soon-1", "access-soon")])
            .await
            .unwrap();

        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].status, AccountStatus::Available.as_str());
        assert_eq!(page.stats.total, 1);
        assert_eq!(page.stats.available, 0);

        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "near expiry stock".to_string(),
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
        assert_eq!(outcome.failures[0].reason, "可兑换账号库存不足");
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
    async fn filtered_account_ids_delete_only_target_pool() {
        let repo = temp_repo().await;
        let left = repo.create_account_pool(pool_input("left")).await.unwrap();
        let right = repo.create_account_pool(pool_input("right")).await.unwrap();
        repo.import_accounts_into_pool(
            &[parsed_account("bulk-target-left", "left-access")],
            Some(&left.id),
        )
        .await
        .unwrap();
        repo.import_accounts_into_pool(
            &[parsed_account("bulk-target-right", "right-access")],
            Some(&right.id),
        )
        .await
        .unwrap();

        let account_ids = repo
            .list_account_ids(AccountListQuery {
                pool_id: Some(left.id.clone()),
                search: Some("bulk-target".to_string()),
                statuses: Some("available".to_string()),
                redeemed_values: Some("false".to_string()),
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(account_ids.len(), 1);

        let outcome = repo.delete_unbound_accounts(&account_ids).await.unwrap();
        assert_eq!(outcome.deleted, 1);

        let left_page = repo
            .list_accounts(AccountListQuery {
                pool_id: Some(left.id),
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let right_page = repo
            .list_accounts(AccountListQuery {
                pool_id: Some(right.id),
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(left_page.total, 0);
        assert_eq!(right_page.total, 1);
        assert_eq!(
            right_page.items[0].account_id.as_deref(),
            Some("bulk-target-right")
        );
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
    async fn delete_redeem_batch_clears_related_data_and_account_bindings() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("acct-1", "access-1"),
            parsed_account("acct-2", "access-2"),
            parsed_account("acct-3", "access-3"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "delete batch".to_string(),
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
        let original_account = page
            .items
            .iter()
            .find(|account| account.redeemed_at.is_some())
            .unwrap()
            .id
            .clone();
        set_account_status(&repo, &original_account, AccountStatus::AuthInvalid).await;
        let prep = repo
            .prepare_after_sale_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        repo.redeem_after_sale_for_export_with_prepared_accounts(
            &[batch.codes[0].code.clone()],
            ExportFormat::Cpa,
            Some(&prep.current_probe_account_ids),
            prep.reservation_id.as_deref(),
            Some(&prep.replacement_probe_account_ids),
        )
        .await
        .unwrap();

        let outcome = repo.delete_redeem_batch(&batch.batch_id).await.unwrap();
        assert!(outcome.deleted);
        assert_eq!(outcome.codes_deleted, 1);
        assert_eq!(outcome.redemptions_deleted, 2);
        assert_eq!(outcome.after_sales_deleted, 1);
        assert_eq!(outcome.accounts_reset, 2);

        let remaining = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(remaining.total, 3);
        assert_eq!(remaining.stats.redeemed, 0);
        assert!(remaining.items.iter().all(|account| {
            account.redeem_code_id.is_none()
                && account.redemption_id.is_none()
                && account.redeemed_at.is_none()
        }));

        for table in [
            "redeem_code_batches",
            "redeem_codes",
            "redeem_redemptions",
            "redeem_after_sales",
        ] {
            let row = sqlx::query(&format!("SELECT COUNT(*) AS count FROM {table}"))
                .fetch_one(repo.pool())
                .await
                .unwrap();
            let count: i64 = row.try_get("count").unwrap();
            assert_eq!(count, 0, "{table} should be empty");
        }
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
    async fn redeem_reserves_buffer_and_refresh_candidates_for_redeem() {
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
        assert_eq!(first_only.refresh_account_ids.len(), 1);
        assert_eq!(first_only.probe_account_ids.len(), 2);
        repo.release_redeem_reservation(first_only.reservation_id.as_deref().unwrap())
            .await
            .unwrap();

        let both = repo
            .prepare_redeem_export(&[batch.codes[0].code.clone(), batch.codes[1].code.clone()])
            .await
            .unwrap();
        assert_eq!(both.estimated_account_count, 2);
        assert_eq!(both.refresh_account_ids.len(), 2);
        assert_eq!(both.probe_account_ids.len(), 3);
        let refreshed = repo
            .load_auth_files_for_ids(&both.refresh_account_ids, false)
            .await
            .unwrap()
            .into_iter()
            .map(|(summary, _)| summary.email.unwrap_or_default())
            .collect::<Vec<_>>();
        assert!(refreshed.iter().all(|email| email.starts_with("expired-")));
    }

    #[tokio::test]
    async fn prepared_redeem_uses_reserved_buffer_after_probe_downgrades_account() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("buffer-1", "access-1"),
            parsed_account("buffer-2", "access-2"),
            parsed_account("buffer-3", "access-3"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "buffered redeem".to_string(),
                total_count: 1,
                accounts_per_code: 2,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let preparation = repo
            .prepare_redeem_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        assert_eq!(preparation.estimated_account_count, 2);
        assert!(preparation.refresh_account_ids.is_empty());
        assert_eq!(preparation.probe_account_ids.len(), 3);

        set_account_status(
            &repo,
            &preparation.probe_account_ids[0],
            AccountStatus::QuotaExhausted,
        )
        .await;
        let outcome = repo
            .redeem_codes_for_export_with_prepared_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                preparation.reservation_id.as_deref(),
                Some(&preparation.probe_account_ids),
            )
            .await
            .unwrap();
        assert_eq!(outcome.successes.len(), 1);
        assert!(outcome.failures.is_empty());
        assert_eq!(outcome.successes[0].account_count, 2);

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
        assert_eq!(
            page.items
                .iter()
                .filter(|account| account.status == AccountStatus::QuotaExhausted.as_str())
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn at_expired_candidate_with_live_token_is_refreshed_for_redeem_prepare() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("stale-status", "access-live")])
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
        repo.mark_account_status(&account_id, AccountStatus::AtExpired)
            .await
            .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "stale status".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let preparation = repo
            .prepare_redeem_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        assert_eq!(preparation.refresh_account_ids, vec![account_id.clone()]);
        assert_eq!(preparation.probe_account_ids, vec![account_id]);
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
        assert!(preparation
            .current_probe_account_ids
            .contains(&old_account.id));

        let after_sale = repo
            .redeem_after_sale_for_export_with_prepared_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                Some(&preparation.current_probe_account_ids),
                preparation.reservation_id.as_deref(),
                Some(&preparation.replacement_probe_account_ids),
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
    async fn prepared_after_sale_rejects_unverified_reserved_replacements() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("old-1", "old-access"),
            parsed_account("fresh-1", "fresh-access"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "after sale verified replacement".to_string(),
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
        let old_account = page
            .items
            .iter()
            .find(|account| account.email.as_deref() == Some("old-1@example.com"))
            .unwrap()
            .clone();
        set_account_status(&repo, &old_account.id, AccountStatus::AuthInvalid).await;

        let preparation = repo
            .prepare_after_sale_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        assert_eq!(preparation.replacement_probe_account_ids.len(), 1);

        let outcome = repo
            .redeem_after_sale_for_export_with_prepared_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                Some(&preparation.current_probe_account_ids),
                preparation.reservation_id.as_deref(),
                Some(&[]),
            )
            .await
            .unwrap();
        assert!(outcome.successes.is_empty());
        assert_eq!(outcome.failures[0].reason, "可补发账号库存不足");
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
            parsed_account("fresh-3", "fresh-access-3"),
            parsed_account("fresh-4", "fresh-access-4"),
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
        let left_code = batch.codes[0].code.clone();
        let right_code = batch.codes[1].code.clone();
        let left_prepare_repo = repo.clone();
        let right_prepare_repo = repo.clone();
        let left_prepare_code = left_code.clone();
        let right_prepare_code = right_code.clone();
        let (left_prep, right_prep) = tokio::join!(
            async move {
                left_prepare_repo
                    .prepare_after_sale_export(&[left_prepare_code])
                    .await
            },
            async move {
                right_prepare_repo
                    .prepare_after_sale_export(&[right_prepare_code])
                    .await
            }
        );
        let left_prep = left_prep.unwrap();
        let right_prep = right_prep.unwrap();
        assert_eq!(left_prep.current_probe_account_ids.len(), 1);
        assert_eq!(right_prep.current_probe_account_ids.len(), 1);
        assert_eq!(left_prep.replacement_probe_account_ids.len(), 2);
        assert_eq!(right_prep.replacement_probe_account_ids.len(), 2);
        assert!(left_prep
            .replacement_probe_account_ids
            .iter()
            .all(|id| !right_prep.replacement_probe_account_ids.contains(id)));

        let left_repo = repo.clone();
        let right_repo = repo.clone();
        let left_current_ids = left_prep.current_probe_account_ids.clone();
        let right_current_ids = right_prep.current_probe_account_ids.clone();
        let left_replacement_ids = left_prep.replacement_probe_account_ids.clone();
        let right_replacement_ids = right_prep.replacement_probe_account_ids.clone();
        let left_reservation_id = left_prep.reservation_id.clone();
        let right_reservation_id = right_prep.reservation_id.clone();
        let (left, right) = tokio::join!(
            async move {
                left_repo
                    .redeem_after_sale_for_export_with_prepared_accounts(
                        &[left_code],
                        ExportFormat::Cpa,
                        Some(&left_current_ids),
                        left_reservation_id.as_deref(),
                        Some(&left_replacement_ids),
                    )
                    .await
            },
            async move {
                right_repo
                    .redeem_after_sale_for_export_with_prepared_accounts(
                        &[right_code],
                        ExportFormat::Cpa,
                        Some(&right_current_ids),
                        right_reservation_id.as_deref(),
                        Some(&right_replacement_ids),
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
    async fn failed_large_demand_does_not_reserve_accounts_for_later_codes() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("acct-1", "access-1"),
            parsed_account("acct-2", "access-2"),
        ])
        .await
        .unwrap();
        let large_batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "large demand".to_string(),
                total_count: 1,
                accounts_per_code: 3,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        let small_batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "small demand".to_string(),
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
                &[
                    large_batch.codes[0].code.clone(),
                    small_batch.codes[0].code.clone(),
                ],
                ExportFormat::Cpa,
            )
            .await
            .unwrap();

        assert_eq!(outcome.successes.len(), 1);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].reason, "可兑换账号库存不足");
        assert_eq!(outcome.successes[0].code, small_batch.codes[0].code);
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
    async fn concurrent_redeems_from_two_repositories_do_not_duplicate_accounts() {
        let database_url = temp_database_url();
        let setup_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        setup_repo
            .import_accounts(&[
                parsed_account("multi-repo-1", "access-1"),
                parsed_account("multi-repo-2", "access-2"),
            ])
            .await
            .unwrap();
        let batch = setup_repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "multi repo concurrent".to_string(),
                total_count: 2,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        drop(setup_repo);

        let left_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let right_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
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

        let verify_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let page = verify_repo
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn stress_concurrent_batch_generation_persists_unique_codes() {
        const BATCH_COUNT: usize = 8;
        const CODES_PER_BATCH: usize = 96;

        let database_url = temp_database_url();
        let setup_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        drop(setup_repo);

        let mut handles = Vec::with_capacity(BATCH_COUNT);
        for batch_index in 0..BATCH_COUNT {
            let database_url = database_url.clone();
            handles.push(tokio::spawn(async move {
                let repo = AccountPoolRepository::connect(&database_url, "test-secret")
                    .await
                    .unwrap();
                repo.create_redeem_batch(CreateRedeemBatchInput {
                    name: format!("stress-generate-{batch_index}"),
                    total_count: CODES_PER_BATCH,
                    accounts_per_code: 1,
                    after_sale_limit: None,
                    expires_at: None,
                    plan_filter: None,
                })
                .await
                .unwrap()
            }));
        }

        let mut batch_ids = HashSet::new();
        let mut generated_hashes = HashSet::new();
        for handle in handles {
            let outcome = handle.await.unwrap();
            assert_eq!(outcome.codes.len(), CODES_PER_BATCH);
            assert!(batch_ids.insert(outcome.batch_id));
            for code in outcome.codes {
                let normalized = normalize_redeem_code(&code.code).unwrap();
                assert_eq!(code.code, format_redeem_code(&normalized));
                assert_eq!(code.masked_code, mask_redeem_code(&normalized));
                assert!(generated_hashes.insert(redeem_code_hash(&normalized)));
            }
        }
        assert_eq!(batch_ids.len(), BATCH_COUNT);
        assert_eq!(generated_hashes.len(), BATCH_COUNT * CODES_PER_BATCH);

        let verify_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let rows = sqlx::query("SELECT batch_id, code_hash FROM redeem_codes")
            .fetch_all(verify_repo.pool())
            .await
            .unwrap();
        assert_eq!(rows.len(), BATCH_COUNT * CODES_PER_BATCH);
        let persisted_hashes = rows
            .iter()
            .map(|row| row.try_get::<String, _>("code_hash").unwrap())
            .collect::<HashSet<_>>();
        assert_eq!(persisted_hashes.len(), BATCH_COUNT * CODES_PER_BATCH);
        assert_eq!(persisted_hashes, generated_hashes);

        let batch_row = sqlx::query(
            "SELECT COUNT(*) AS count, SUM(total_count) AS total FROM redeem_code_batches",
        )
        .fetch_one(verify_repo.pool())
        .await
        .unwrap();
        let batch_count: i64 = batch_row.try_get("count").unwrap();
        let total_count: i64 = batch_row.try_get("total").unwrap();
        assert_eq!(batch_count, BATCH_COUNT as i64);
        assert_eq!(total_count, (BATCH_COUNT * CODES_PER_BATCH) as i64);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn stress_concurrent_redeems_allocate_distinct_accounts() {
        const CODE_COUNT: usize = 64;
        const ACCOUNTS_PER_CODE: usize = 2;
        const ACCOUNT_COUNT: usize = CODE_COUNT * ACCOUNTS_PER_CODE;

        let database_url = temp_database_url();
        let setup_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let accounts = (0..ACCOUNT_COUNT)
            .map(|index| {
                parsed_account(
                    &format!("stress-account-{index:03}"),
                    &format!("stress-access-{index:03}"),
                )
            })
            .collect::<Vec<_>>();
        setup_repo.import_accounts(&accounts).await.unwrap();
        let batch = setup_repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "stress-redeem".to_string(),
                total_count: CODE_COUNT,
                accounts_per_code: ACCOUNTS_PER_CODE,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        drop(setup_repo);

        let mut handles = Vec::with_capacity(CODE_COUNT);
        for created in batch.codes {
            let database_url = database_url.clone();
            let code = created.code;
            handles.push(tokio::spawn(async move {
                let repo = AccountPoolRepository::connect(&database_url, "test-secret")
                    .await
                    .unwrap();
                repo.redeem_codes_for_export(&[code], ExportFormat::Sub2api)
                    .await
                    .unwrap()
            }));
        }

        let mut exported_tokens = HashSet::new();
        for handle in handles {
            let outcome = handle.await.unwrap();
            assert!(outcome.failures.is_empty());
            assert_eq!(outcome.successes.len(), 1);
            assert_eq!(outcome.successes[0].account_count, ACCOUNTS_PER_CODE);
            let tokens = document_access_tokens(&outcome.document);
            assert_eq!(tokens.len(), ACCOUNTS_PER_CODE);
            for token in tokens {
                assert!(exported_tokens.insert(token), "duplicate exported token");
            }
        }
        assert_eq!(exported_tokens.len(), ACCOUNT_COUNT);

        let verify_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let page = verify_repo
            .list_accounts(AccountListQuery {
                limit: ACCOUNT_COUNT,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, ACCOUNT_COUNT);
        assert_eq!(page.stats.redeemed, ACCOUNT_COUNT);
        assert_eq!(
            page.items
                .iter()
                .filter(|account| account.redeemed_at.is_some())
                .count(),
            ACCOUNT_COUNT
        );

        let redemption_rows =
            sqlx::query("SELECT account_ids_json FROM redeem_redemptions ORDER BY created_at ASC")
                .fetch_all(verify_repo.pool())
                .await
                .unwrap();
        assert_eq!(redemption_rows.len(), CODE_COUNT);
        let mut redeemed_account_ids = HashSet::new();
        for row in redemption_rows {
            let account_ids = serde_json::from_str::<Vec<String>>(
                row.try_get::<String, _>("account_ids_json")
                    .unwrap()
                    .as_str(),
            )
            .unwrap();
            assert_eq!(account_ids.len(), ACCOUNTS_PER_CODE);
            for account_id in account_ids {
                assert!(
                    redeemed_account_ids.insert(account_id),
                    "duplicate account id"
                );
            }
        }
        assert_eq!(redeemed_account_ids.len(), ACCOUNT_COUNT);

        let code_row = sqlx::query(
            "SELECT COUNT(*) AS count FROM redeem_codes WHERE status = 'redeemed' AND redemption_id IS NOT NULL",
        )
        .fetch_one(verify_repo.pool())
        .await
        .unwrap();
        let redeemed_codes: i64 = code_row.try_get("count").unwrap();
        assert_eq!(redeemed_codes, CODE_COUNT as i64);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn stress_duplicate_redeem_requests_return_one_binding_snapshot() {
        const ATTEMPT_COUNT: usize = 40;

        let database_url = temp_database_url();
        let setup_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        setup_repo
            .import_accounts(&[parsed_account("duplicate-stress", "duplicate-access")])
            .await
            .unwrap();
        let batch = setup_repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "duplicate stress".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        let code = batch.codes[0].code.clone();
        drop(setup_repo);

        let mut handles = Vec::with_capacity(ATTEMPT_COUNT);
        for _ in 0..ATTEMPT_COUNT {
            let database_url = database_url.clone();
            let code = code.clone();
            handles.push(tokio::spawn(async move {
                let repo = AccountPoolRepository::connect(&database_url, "test-secret")
                    .await
                    .unwrap();
                repo.redeem_codes_for_export(&[code], ExportFormat::Cpa)
                    .await
                    .unwrap()
            }));
        }

        for handle in handles {
            let outcome = handle.await.unwrap();
            assert!(outcome.failures.is_empty());
            assert_eq!(outcome.successes.len(), 1);
            assert_eq!(outcome.successes[0].account_count, 1);
            assert_eq!(document_access_token(&outcome.document), "duplicate-access");
        }

        let verify_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let redemption_count_row = sqlx::query("SELECT COUNT(*) AS count FROM redeem_redemptions")
            .fetch_one(verify_repo.pool())
            .await
            .unwrap();
        let redemption_count: i64 = redemption_count_row.try_get("count").unwrap();
        assert_eq!(redemption_count, 1);

        let account_count_row =
            sqlx::query("SELECT COUNT(*) AS count FROM accounts WHERE redeemed_at IS NOT NULL")
                .fetch_one(verify_repo.pool())
                .await
                .unwrap();
        let account_count: i64 = account_count_row.try_get("count").unwrap();
        assert_eq!(account_count, 1);

        let code_row = sqlx::query(
            "SELECT status, redeemed_at, redemption_id FROM redeem_codes WHERE code_hash = ?",
        )
        .bind(redeem_code_hash(&normalize_redeem_code(&code).unwrap()))
        .fetch_one(verify_repo.pool())
        .await
        .unwrap();
        let status: String = code_row.try_get("status").unwrap();
        let redeemed_at: Option<i64> = code_row.try_get("redeemed_at").unwrap();
        let redemption_id: Option<String> = code_row.try_get("redemption_id").unwrap();
        assert_eq!(status, "redeemed");
        assert!(redeemed_at.is_some());
        assert!(redemption_id.is_some());

        let batch_row = sqlx::query("SELECT redeemed_count FROM redeem_code_batches WHERE id = ?")
            .bind(&batch.batch_id)
            .fetch_one(verify_repo.pool())
            .await
            .unwrap();
        let redeemed_count: i64 = batch_row.try_get("redeemed_count").unwrap();
        assert_eq!(redeemed_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 16)]
    #[ignore = "run with make test-stress-high"]
    async fn stress_high_concurrency_redeem_speed_and_correctness() {
        let code_count = stress_env_usize("AETHER_STRESS_CODES", 10_000, 1, 100_000);
        let accounts_per_code = stress_env_usize("AETHER_STRESS_ACCOUNTS_PER_CODE", 1, 1, 10);
        let account_count = code_count * accounts_per_code;
        let concurrency = stress_env_usize("AETHER_STRESS_CONCURRENCY", 200, 1, 5_000);
        let chunk_size = stress_env_usize("AETHER_STRESS_CHUNK_SIZE", 50, 1, 1_000);
        let min_codes_per_sec =
            stress_env_usize("AETHER_STRESS_MIN_CODES_PER_SEC", 1_500, 1, usize::MAX) as f64;

        let database_url = temp_database_url();
        let setup_started = std::time::Instant::now();
        let repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let accounts = (0..account_count)
            .map(|index| {
                parsed_account(
                    &format!("high-stress-account-{index:06}"),
                    &format!("high-stress-access-{index:06}"),
                )
            })
            .collect::<Vec<_>>();
        let import_outcome = repo.import_accounts(&accounts).await.unwrap();
        assert_eq!(import_outcome.imported, account_count);
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "high concurrency redeem speed".to_string(),
                total_count: code_count,
                accounts_per_code,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        assert_eq!(batch.codes.len(), code_count);
        let setup_elapsed = setup_started.elapsed();

        let chunks = batch
            .codes
            .chunks(chunk_size)
            .map(|chunk| {
                chunk
                    .iter()
                    .map(|created| created.code.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let request_count = chunks.len();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
        let redeem_started = std::time::Instant::now();
        let mut handles = Vec::with_capacity(request_count);
        for codes in chunks {
            let repo = repo.clone();
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            handles.push(tokio::spawn(async move {
                let _permit = permit;
                let chunk_started = std::time::Instant::now();
                let outcome = repo
                    .redeem_codes_for_export(&codes, ExportFormat::Sub2api)
                    .await
                    .unwrap();
                let elapsed = chunk_started.elapsed();
                let failures = outcome
                    .failures
                    .into_iter()
                    .map(|failure| format!("{}: {}", failure.code, failure.reason))
                    .collect::<Vec<_>>();
                (
                    outcome.successes.len(),
                    failures,
                    document_access_tokens(&outcome.document),
                    elapsed,
                )
            }));
        }

        let mut success_count = 0;
        let mut failures = Vec::new();
        let mut exported_tokens = HashSet::new();
        let mut chunk_durations = Vec::new();
        for handle in handles {
            let (chunk_success_count, chunk_failures, tokens, elapsed) = handle.await.unwrap();
            success_count += chunk_success_count;
            failures.extend(chunk_failures);
            chunk_durations.push(elapsed);
            for token in tokens {
                assert!(exported_tokens.insert(token), "duplicate exported token");
            }
        }
        let redeem_elapsed = redeem_started.elapsed();
        let codes_per_sec = code_count as f64 / redeem_elapsed.as_secs_f64();
        let accounts_per_sec = account_count as f64 / redeem_elapsed.as_secs_f64();
        eprintln!(
            "high-stress redeem: codes={code_count} accounts={account_count} requests={request_count} concurrency={concurrency} chunk_size={chunk_size} setup={:.3}s redeem={:.3}s throughput={:.0} codes/s {:.0} accounts/s p50={:.1}ms p95={:.1}ms p99={:.1}ms",
            setup_elapsed.as_secs_f64(),
            redeem_elapsed.as_secs_f64(),
            codes_per_sec,
            accounts_per_sec,
            percentile_ms(&chunk_durations, 50),
            percentile_ms(&chunk_durations, 95),
            percentile_ms(&chunk_durations, 99),
        );

        assert!(failures.is_empty(), "redeem failures: {failures:?}");
        assert_eq!(success_count, code_count);
        assert_eq!(exported_tokens.len(), account_count);
        assert!(
            codes_per_sec >= min_codes_per_sec,
            "redeem throughput {codes_per_sec:.0} codes/s is below required {min_codes_per_sec:.0} codes/s"
        );

        let summary_row = sqlx::query(
            r#"
SELECT
  (SELECT COUNT(*) FROM accounts WHERE redeemed_at IS NOT NULL) AS redeemed_accounts,
  (SELECT COUNT(*) FROM redeem_redemptions) AS redemptions,
  (SELECT COUNT(*) FROM redeem_codes WHERE status = 'redeemed' AND redemption_id IS NOT NULL) AS redeemed_codes,
  (SELECT redeemed_count FROM redeem_code_batches WHERE id = ?) AS redeemed_count
"#,
        )
        .bind(&batch.batch_id)
        .fetch_one(repo.pool())
        .await
        .unwrap();
        let redeemed_accounts: i64 = summary_row.try_get("redeemed_accounts").unwrap();
        let redemptions: i64 = summary_row.try_get("redemptions").unwrap();
        let redeemed_codes: i64 = summary_row.try_get("redeemed_codes").unwrap();
        let redeemed_count: i64 = summary_row.try_get("redeemed_count").unwrap();
        assert_eq!(redeemed_accounts, account_count as i64);
        assert_eq!(redemptions, code_count as i64);
        assert_eq!(redeemed_codes, code_count as i64);
        assert_eq!(redeemed_count, code_count as i64);

        let redemption_rows = sqlx::query("SELECT account_ids_json FROM redeem_redemptions")
            .fetch_all(repo.pool())
            .await
            .unwrap();
        let mut redeemed_account_ids = HashSet::new();
        for row in redemption_rows {
            let account_ids = serde_json::from_str::<Vec<String>>(
                row.try_get::<String, _>("account_ids_json")
                    .unwrap()
                    .as_str(),
            )
            .unwrap();
            assert_eq!(account_ids.len(), accounts_per_code);
            for account_id in account_ids {
                assert!(
                    redeemed_account_ids.insert(account_id),
                    "duplicate redeemed account id"
                );
            }
        }
        assert_eq!(redeemed_account_ids.len(), account_count);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 16)]
    #[ignore = "run with make test-stress-high"]
    async fn stress_high_duplicate_redeem_speed_and_idempotence() {
        let attempt_count = stress_env_usize("AETHER_DUPLICATE_STRESS_ATTEMPTS", 1_000, 1, 100_000);
        let concurrency = stress_env_usize("AETHER_DUPLICATE_STRESS_CONCURRENCY", 256, 1, 10_000);
        let min_attempts_per_sec = stress_env_usize(
            "AETHER_DUPLICATE_STRESS_MIN_ATTEMPTS_PER_SEC",
            2_000,
            1,
            usize::MAX,
        ) as f64;

        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account(
            "high-duplicate-stress",
            "high-duplicate-access",
        )])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "high duplicate redeem speed".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        let code = batch.codes[0].code.clone();

        let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
        let redeem_started = std::time::Instant::now();
        let mut handles = Vec::with_capacity(attempt_count);
        for _ in 0..attempt_count {
            let repo = repo.clone();
            let code = code.clone();
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            handles.push(tokio::spawn(async move {
                let _permit = permit;
                let attempt_started = std::time::Instant::now();
                let outcome = repo
                    .redeem_codes_for_export(&[code], ExportFormat::Cpa)
                    .await
                    .unwrap();
                (
                    outcome.successes.len(),
                    outcome.failures.len(),
                    document_access_token(&outcome.document),
                    attempt_started.elapsed(),
                )
            }));
        }

        let mut success_count = 0;
        let mut failure_count = 0;
        let mut attempt_durations = Vec::new();
        for handle in handles {
            let (successes, failures, token, elapsed) = handle.await.unwrap();
            success_count += successes;
            failure_count += failures;
            assert_eq!(token, "high-duplicate-access");
            attempt_durations.push(elapsed);
        }
        let redeem_elapsed = redeem_started.elapsed();
        let attempts_per_sec = attempt_count as f64 / redeem_elapsed.as_secs_f64();
        eprintln!(
            "high-duplicate redeem: attempts={attempt_count} concurrency={concurrency} elapsed={:.3}s throughput={:.0} attempts/s p50={:.1}ms p95={:.1}ms p99={:.1}ms",
            redeem_elapsed.as_secs_f64(),
            attempts_per_sec,
            percentile_ms(&attempt_durations, 50),
            percentile_ms(&attempt_durations, 95),
            percentile_ms(&attempt_durations, 99),
        );

        assert_eq!(success_count, attempt_count);
        assert_eq!(failure_count, 0);
        assert!(
            attempts_per_sec >= min_attempts_per_sec,
            "duplicate redeem throughput {attempts_per_sec:.0} attempts/s is below required {min_attempts_per_sec:.0} attempts/s"
        );

        let row = sqlx::query(
            r#"
SELECT
  (SELECT COUNT(*) FROM redeem_redemptions) AS redemptions,
  (SELECT COUNT(*) FROM accounts WHERE redeemed_at IS NOT NULL) AS redeemed_accounts,
  (SELECT redeemed_count FROM redeem_code_batches WHERE id = ?) AS redeemed_count,
  (SELECT COUNT(*) FROM account_exports WHERE source = 'redeem') AS export_count
"#,
        )
        .bind(&batch.batch_id)
        .fetch_one(repo.pool())
        .await
        .unwrap();
        let redemptions: i64 = row.try_get("redemptions").unwrap();
        let redeemed_accounts: i64 = row.try_get("redeemed_accounts").unwrap();
        let redeemed_count: i64 = row.try_get("redeemed_count").unwrap();
        let export_count: i64 = row.try_get("export_count").unwrap();
        assert_eq!(redemptions, 1);
        assert_eq!(redeemed_accounts, 1);
        assert_eq!(redeemed_count, 1);
        assert_eq!(export_count, attempt_count as i64);
    }

    #[tokio::test]
    async fn concurrent_prepared_redeems_reserve_distinct_accounts() {
        let database_url = temp_database_url();
        let setup_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        setup_repo
            .import_accounts(&[
                parsed_account("prepared-1", "access-1"),
                parsed_account("prepared-2", "access-2"),
                parsed_account("prepared-3", "access-3"),
                parsed_account("prepared-4", "access-4"),
            ])
            .await
            .unwrap();
        let batch = setup_repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "prepared concurrent".to_string(),
                total_count: 2,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        drop(setup_repo);

        let left_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let right_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let left_code = batch.codes[0].code.clone();
        let right_code = batch.codes[1].code.clone();
        let left_prepare_code = left_code.clone();
        let right_prepare_code = right_code.clone();
        let (left_prep, right_prep) = tokio::join!(
            async { left_repo.prepare_redeem_export(&[left_prepare_code]).await },
            async {
                right_repo
                    .prepare_redeem_export(&[right_prepare_code])
                    .await
            }
        );
        let left_prep = left_prep.unwrap();
        let right_prep = right_prep.unwrap();
        assert_eq!(left_prep.probe_account_ids.len(), 2);
        assert_eq!(right_prep.probe_account_ids.len(), 2);
        assert!(left_prep
            .probe_account_ids
            .iter()
            .all(|id| !right_prep.probe_account_ids.contains(id)));

        let left_verified_ids = left_prep.probe_account_ids.clone();
        let right_verified_ids = right_prep.probe_account_ids.clone();
        let left_reservation_id = left_prep.reservation_id.clone();
        let right_reservation_id = right_prep.reservation_id.clone();
        let (left, right) = tokio::join!(
            async {
                left_repo
                    .redeem_codes_for_export_with_prepared_accounts(
                        &[left_code],
                        ExportFormat::Cpa,
                        left_reservation_id.as_deref(),
                        Some(&left_verified_ids),
                    )
                    .await
            },
            async {
                right_repo
                    .redeem_codes_for_export_with_prepared_accounts(
                        &[right_code],
                        ExportFormat::Cpa,
                        right_reservation_id.as_deref(),
                        Some(&right_verified_ids),
                    )
                    .await
            }
        );

        let left_token = document_access_token(&left.unwrap().document);
        let right_token = document_access_token(&right.unwrap().document);
        assert_ne!(left_token, right_token);

        let verify_repo = AccountPoolRepository::connect(&database_url, "test-secret")
            .await
            .unwrap();
        let page = verify_repo
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
        let row = sqlx::query(
            "SELECT COUNT(*) AS count FROM accounts WHERE redeem_reservation_id IS NOT NULL",
        )
        .fetch_one(verify_repo.pool())
        .await
        .unwrap();
        let reservation_count: i64 = row.try_get("count").unwrap();
        assert_eq!(reservation_count, 0);
    }

    #[tokio::test]
    async fn unexpired_redeem_reservation_blocks_other_prepare() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("reserved-1", "access-1")])
            .await
            .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "reservation blocks".to_string(),
                total_count: 2,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let first = repo
            .prepare_redeem_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        assert_eq!(first.probe_account_ids.len(), 1);

        let second = repo
            .prepare_redeem_export(&[batch.codes[1].code.clone()])
            .await
            .unwrap();
        assert!(second.probe_account_ids.is_empty());
        let blocked = repo
            .redeem_codes_for_export_with_prepared_accounts(
                &[batch.codes[1].code.clone()],
                ExportFormat::Cpa,
                second.reservation_id.as_deref(),
                Some(&second.probe_account_ids),
            )
            .await
            .unwrap();
        assert!(blocked.successes.is_empty());
        assert_eq!(blocked.failures[0].reason, "可兑换账号库存不足");
        repo.release_redeem_reservation(first.reservation_id.as_deref().unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn expired_redeem_reservation_can_be_reclaimed() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("reclaim-1", "access-1")])
            .await
            .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "reservation reclaim".to_string(),
                total_count: 2,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();

        let first = repo
            .prepare_redeem_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        assert_eq!(first.probe_account_ids.len(), 1);
        let expired_at = unix_now_secs().saturating_sub(REDEEM_RESERVATION_TTL_SECONDS + 1) as i64;
        sqlx::query("UPDATE accounts SET redeem_reserved_at = ? WHERE redeem_reservation_id = ?")
            .bind(expired_at)
            .bind(first.reservation_id.as_deref().unwrap())
            .execute(repo.pool())
            .await
            .unwrap();

        let second = repo
            .prepare_redeem_export(&[batch.codes[1].code.clone()])
            .await
            .unwrap();
        assert_eq!(second.probe_account_ids.len(), 1);
        assert_eq!(second.probe_account_ids[0], first.probe_account_ids[0]);
        repo.release_redeem_reservation(second.reservation_id.as_deref().unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn refresh_redeem_reservation_extends_reserved_at() {
        let repo = temp_repo().await;
        repo.import_accounts(&[parsed_account("heartbeat-1", "access-1")])
            .await
            .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "reservation heartbeat".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        let preparation = repo
            .prepare_redeem_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        let reservation_id = preparation.reservation_id.as_deref().unwrap();
        sqlx::query("UPDATE accounts SET redeem_reserved_at = 1 WHERE redeem_reservation_id = ?")
            .bind(reservation_id)
            .execute(repo.pool())
            .await
            .unwrap();

        repo.refresh_redeem_reservation(reservation_id)
            .await
            .unwrap();

        let row =
            sqlx::query("SELECT redeem_reserved_at FROM accounts WHERE redeem_reservation_id = ?")
                .bind(reservation_id)
                .fetch_one(repo.pool())
                .await
                .unwrap();
        let reserved_at: i64 = row.try_get("redeem_reserved_at").unwrap();
        assert!(reserved_at > 1);
        repo.release_redeem_reservation(reservation_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn prepared_redeem_rejects_unreserved_verified_accounts() {
        let repo = temp_repo().await;
        repo.import_accounts(&[
            parsed_account("reserved-scope-1", "access-1"),
            parsed_account("reserved-scope-2", "access-2"),
            parsed_account("reserved-scope-3", "access-3"),
        ])
        .await
        .unwrap();
        let batch = repo
            .create_redeem_batch(CreateRedeemBatchInput {
                name: "reservation scope".to_string(),
                total_count: 1,
                accounts_per_code: 1,
                after_sale_limit: None,
                expires_at: None,
                plan_filter: None,
            })
            .await
            .unwrap();
        let preparation = repo
            .prepare_redeem_export(&[batch.codes[0].code.clone()])
            .await
            .unwrap();
        let page = repo
            .list_accounts(AccountListQuery {
                limit: 10,
                ..AccountListQuery::default()
            })
            .await
            .unwrap();
        let unreserved_id = page
            .items
            .iter()
            .find(|account| !preparation.probe_account_ids.contains(&account.id))
            .unwrap()
            .id
            .clone();
        let outcome = repo
            .redeem_codes_for_export_with_prepared_accounts(
                &[batch.codes[0].code.clone()],
                ExportFormat::Cpa,
                preparation.reservation_id.as_deref(),
                Some(&[unreserved_id]),
            )
            .await
            .unwrap();
        assert!(outcome.successes.is_empty());
        assert_eq!(outcome.failures[0].reason, "可兑换账号库存不足");
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
