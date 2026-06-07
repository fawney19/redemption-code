use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub const CODEX_WHAM_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const CHATGPT_ACCOUNTS_CHECK_URL: &str =
    "https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27?timezone_offset_min=-480";
pub const CHATGPT_SESSION_URL: &str = "https://chatgpt.com/api/auth/session";
pub const OPENAI_OAUTH_REFRESH_SCOPE: &str = "openid profile email";
pub const OPENAI_BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";
pub const CPA_PROBE_USER_AGENT: &str =
    "codex_cli_rs/0.76.0 (Debian 13.0.0; x86_64) WindowsTerminal";
pub const ACCESS_TOKEN_REFRESH_GRACE_SECONDS: u64 = 10 * 60;
pub const REDEEMED_ACCOUNT_DELETABLE_STATUSES: &[&str] =
    &["at_expired", "refresh_failed", "auth_invalid", "forbidden"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    Available,
    AtExpired,
    RefreshFailed,
    AuthInvalid,
    Forbidden,
    QuotaExhausted,
    Redeemed,
}

impl AccountStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::AtExpired => "at_expired",
            Self::RefreshFailed => "refresh_failed",
            Self::AuthInvalid => "auth_invalid",
            Self::Forbidden => "forbidden",
            Self::QuotaExhausted => "quota_exhausted",
            Self::Redeemed => "redeemed",
        }
    }
}

impl std::str::FromStr for AccountStatus {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "available" => Ok(Self::Available),
            "at_expired" => Ok(Self::AtExpired),
            "refresh_failed" => Ok(Self::RefreshFailed),
            "auth_invalid" => Ok(Self::AuthInvalid),
            "forbidden" => Ok(Self::Forbidden),
            "quota_exhausted" => Ok(Self::QuotaExhausted),
            "redeemed" => Ok(Self::Redeemed),
            _ => Err(()),
        }
    }
}

pub fn is_redeemed_account_deletable_status(status: &str) -> bool {
    REDEEMED_ACCOUNT_DELETABLE_STATUSES
        .iter()
        .any(|allowed| status.trim().eq_ignore_ascii_case(allowed))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Cpa,
    Sub2api,
}

impl ExportFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpa => "cpa",
            Self::Sub2api => "sub2api",
        }
    }
}

impl std::str::FromStr for ExportFormat {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cpa" => Ok(Self::Cpa),
            "sub2api" | "sub2" => Ok(Self::Sub2api),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CodexAuthFile {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chatgpt_account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chatgpt_plan_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expired: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CodexAuthFile {
    pub fn normalized(mut self) -> Self {
        self.kind.get_or_insert_with(|| "codex".to_string());
        if self.chatgpt_account_id.is_none() {
            self.chatgpt_account_id = self.account_id.clone();
        }
        if self.account_id.is_none() {
            self.account_id = self.chatgpt_account_id.clone();
        }
        if self.chatgpt_plan_type.is_none() {
            self.chatgpt_plan_type = self.plan_type.clone();
        }
        if self.plan_type.is_none() {
            self.plan_type = self.chatgpt_plan_type.clone();
        }
        if self.name.is_none() {
            self.name = self.email.clone();
        }
        self
    }

    pub fn to_cpa_value(&self) -> Value {
        serde_json::to_value(self.clone().normalized()).unwrap_or_else(|_| json!({}))
    }

    pub fn expires_at_epoch(&self) -> Option<u64> {
        epoch_seconds_from_value(self.expires_at.as_ref())
            .or_else(|| epoch_seconds_from_value(self.expired.as_ref()))
            .or_else(|| {
                self.access_token
                    .as_deref()
                    .and_then(decode_access_token_expires_at)
            })
    }

    pub fn fingerprint_material(&self) -> String {
        let workspace_id = normalized_string(
            self.account_id
                .as_deref()
                .or(self.chatgpt_account_id.as_deref()),
        );
        if let Some(email) = normalized_email(self.email.as_deref()) {
            return match workspace_id {
                Some(workspace_id) => format!("email:{email}|workspace:{workspace_id}"),
                None => format!("email:{email}"),
            };
        }
        if let Some(subject) = token_subject(self.id_token.as_deref())
            .or_else(|| token_subject(self.access_token.as_deref()))
        {
            return match workspace_id {
                Some(workspace_id) => format!("subject:{subject}|workspace:{workspace_id}"),
                None => format!("subject:{subject}"),
            };
        }
        self.refresh_token
            .as_deref()
            .and_then(|value| normalized_string(Some(value)))
            .map(|value| format!("refresh:{value}"))
            .or_else(|| {
                self.access_token
                    .as_deref()
                    .and_then(|value| normalized_string(Some(value)))
                    .map(|value| format!("access:{value}"))
            })
            .unwrap_or_else(|| serde_json::to_string(&self.to_cpa_value()).unwrap_or_default())
    }

    pub fn legacy_fingerprint_material(&self) -> String {
        self.account_id
            .as_ref()
            .or(self.chatgpt_account_id.as_ref())
            .or(self.email.as_ref())
            .or(self.refresh_token.as_ref())
            .or(self.access_token.as_ref())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| serde_json::to_string(&self.to_cpa_value()).unwrap_or_default())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedAccount {
    pub auth_file: CodexAuthFile,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportParseResult {
    pub accounts: Vec<ParsedAccount>,
    pub errors: Vec<String>,
}

pub fn parse_codex_accounts(input: &str) -> ImportParseResult {
    let mut accounts = Vec::new();
    let mut errors = Vec::new();
    let clean = input.trim();
    if clean.is_empty() {
        return ImportParseResult { accounts, errors };
    }

    match serde_json::from_str::<Value>(clean) {
        Ok(value) => extract_accounts_from_value(&value, "json", &mut accounts, &mut errors),
        Err(_) => {
            for (index, line) in clean.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(value) => {
                        extract_accounts_from_value(&value, "jsonl", &mut accounts, &mut errors)
                    }
                    Err(_) => match auth_file_from_raw_token(line) {
                        Some(auth_file) => accounts.push(ParsedAccount {
                            auth_file,
                            source: "raw_token".to_string(),
                        }),
                        None => errors.push(format!("line {} could not be parsed", index + 1)),
                    },
                }
            }
        }
    }

    ImportParseResult { accounts, errors }
}

fn extract_accounts_from_value(
    value: &Value,
    source: &str,
    accounts: &mut Vec<ParsedAccount>,
    errors: &mut Vec<String>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                extract_accounts_from_value(item, source, accounts, errors);
            }
        }
        Value::Object(object) => {
            if let Some(items) = object
                .get("accounts")
                .or_else(|| object.get("items"))
                .and_then(Value::as_array)
            {
                for item in items {
                    extract_accounts_from_value(item, source, accounts, errors);
                }
                return;
            }
            if let Some(auth_file) = object.get("auth_file").and_then(auth_file_from_value) {
                accounts.push(ParsedAccount {
                    auth_file,
                    source: source.to_string(),
                });
                return;
            }
            if let Some(credentials) = object.get("credentials") {
                let mut merged = credentials.clone();
                if let Some(merged_object) = merged.as_object_mut() {
                    for key in [
                        "email",
                        "name",
                        "plan_type",
                        "account_id",
                        "chatgpt_account_id",
                    ] {
                        if !merged_object.contains_key(key) {
                            if let Some(value) = object.get(key) {
                                merged_object.insert(key.to_string(), value.clone());
                            }
                        }
                    }
                }
                if let Some(auth_file) = auth_file_from_value(&merged) {
                    accounts.push(ParsedAccount {
                        auth_file,
                        source: "sub2api".to_string(),
                    });
                    return;
                }
            }
            if let Some(auth_file) = auth_file_from_value(value) {
                accounts.push(ParsedAccount {
                    auth_file,
                    source: source.to_string(),
                });
            } else {
                errors.push("json object does not contain usable codex credentials".to_string());
            }
        }
        Value::String(raw) => {
            if let Some(auth_file) = auth_file_from_raw_token(raw) {
                accounts.push(ParsedAccount {
                    auth_file,
                    source: source.to_string(),
                });
            }
        }
        _ => errors.push("unsupported credential JSON value".to_string()),
    }
}

fn auth_file_from_value(value: &Value) -> Option<CodexAuthFile> {
    let object = value.as_object()?;
    let mut auth_file = CodexAuthFile {
        kind: string_field(object, &["type"]).or_else(|| Some("codex".to_string())),
        account_id: string_field(object, &["account_id", "accountId"]),
        chatgpt_account_id: string_field(object, &["chatgpt_account_id", "chatgptAccountId"]),
        email: string_field(object, &["email", "oauth_email"]),
        name: string_field(object, &["name", "account_name", "accountName"]),
        plan_type: string_field(object, &["plan_type", "planType"]),
        chatgpt_plan_type: string_field(object, &["chatgpt_plan_type", "chatgptPlanType"]),
        id_token: string_field(object, &["id_token", "idToken"]),
        access_token: string_field(object, &["access_token", "accessToken", "token"]),
        refresh_token: string_field(object, &["refresh_token", "refreshToken"]),
        session_token: string_field(object, &["session_token", "sessionToken"]),
        last_refresh: string_field(object, &["last_refresh", "lastRefresh"]),
        expired: object.get("expired").cloned(),
        expires_at: object
            .get("expires_at")
            .or_else(|| object.get("expiresAt"))
            .cloned(),
        ..CodexAuthFile::default()
    };

    for (key, value) in object {
        if !matches!(
            key.as_str(),
            "type"
                | "account_id"
                | "accountId"
                | "chatgpt_account_id"
                | "chatgptAccountId"
                | "email"
                | "oauth_email"
                | "name"
                | "account_name"
                | "accountName"
                | "plan_type"
                | "planType"
                | "chatgpt_plan_type"
                | "chatgptPlanType"
                | "id_token"
                | "idToken"
                | "access_token"
                | "accessToken"
                | "token"
                | "refresh_token"
                | "refreshToken"
                | "session_token"
                | "sessionToken"
                | "last_refresh"
                | "lastRefresh"
                | "expired"
                | "expires_at"
                | "expiresAt"
        ) {
            auth_file.extra.insert(key.clone(), value.clone());
        }
    }

    if auth_file.access_token.is_none() && auth_file.refresh_token.is_none() {
        return None;
    }
    if auth_file.email.is_none() || auth_file.account_id.is_none() || auth_file.expires_at.is_none()
    {
        enrich_from_access_token(&mut auth_file);
    }
    Some(auth_file.normalized())
}

fn auth_file_from_raw_token(raw: &str) -> Option<CodexAuthFile> {
    let token = raw.trim();
    if token.is_empty() {
        return None;
    }
    let mut auth_file = CodexAuthFile {
        kind: Some("codex".to_string()),
        ..CodexAuthFile::default()
    };
    if looks_like_access_token(token) {
        auth_file.access_token = Some(token.to_string());
    } else {
        auth_file.refresh_token = Some(token.to_string());
    }
    enrich_from_access_token(&mut auth_file);
    Some(auth_file.normalized())
}

fn enrich_from_access_token(auth_file: &mut CodexAuthFile) {
    let Some(access_token) = auth_file.access_token.as_deref() else {
        return;
    };
    let Some(claims) = decode_access_token_claims(access_token) else {
        return;
    };
    if auth_file.expires_at.is_none() {
        if let Some(exp) = claims.get("exp").and_then(json_u64_value) {
            auth_file.expires_at = Some(json!(exp));
        }
    }
    if auth_file.email.is_none() {
        auth_file.email = nested_string_field(
            &claims,
            &[
                &["https://api.openai.com/profile", "email"][..],
                &["email"][..],
            ],
        );
    }
    if auth_file.account_id.is_none() {
        auth_file.account_id = nested_string_field(
            &claims,
            &[
                &["https://api.openai.com/auth", "chatgpt_account_id"][..],
                &["chatgpt_account_id"][..],
                &["account_id"][..],
            ],
        );
    }
}

fn string_field(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn normalized_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalized_email(value: Option<&str>) -> Option<String> {
    normalized_string(value).map(|value| value.to_ascii_lowercase())
}

fn token_subject(token: Option<&str>) -> Option<String> {
    let claims = decode_access_token_claims(token?)?;
    nested_string_field(
        &claims,
        &[
            &["sub"][..],
            &["user_id"][..],
            &["https://api.openai.com/auth", "user_id"][..],
            &["https://api.openai.com/profile", "user_id"][..],
        ],
    )
}

fn nested_string_field(object: &Map<String, Value>, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| {
        let mut current = Value::Object(object.clone());
        for segment in *path {
            current = current.get(*segment)?.clone();
        }
        current
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

pub fn epoch_seconds_from_value(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64().map(normalize_epoch_seconds),
        Value::String(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return None;
            }
            raw.parse::<u64>()
                .ok()
                .map(normalize_epoch_seconds)
                .or_else(|| {
                    DateTime::parse_from_rfc3339(raw)
                        .ok()
                        .and_then(|value| u64::try_from(value.timestamp()).ok())
                })
        }
        _ => None,
    }
}

fn normalize_epoch_seconds(value: u64) -> u64 {
    if value > 100_000_000_000 {
        value / 1000
    } else {
        value
    }
}

pub fn access_token_needs_refresh(
    expires_at: Option<u64>,
    now_unix_secs: u64,
    grace_seconds: u64,
) -> bool {
    expires_at.is_some_and(|expires_at| expires_at <= now_unix_secs.saturating_add(grace_seconds))
}

pub fn fingerprint_auth_file(auth_file: &CodexAuthFile) -> String {
    let mut hasher = Sha256::new();
    hasher.update(auth_file.fingerprint_material().as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn legacy_fingerprint_auth_file(auth_file: &CodexAuthFile) -> String {
    let mut hasher = Sha256::new();
    hasher.update(auth_file.legacy_fingerprint_material().as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn secret_preview(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if value.len() <= 12 {
        return Some(format!("{}...", &value[..value.len().min(4)]));
    }
    Some(format!("{}...{}", &value[..6], &value[value.len() - 4..]))
}

pub fn export_accounts(format: ExportFormat, accounts: &[CodexAuthFile]) -> Value {
    match format {
        ExportFormat::Cpa => export_cpa(accounts),
        ExportFormat::Sub2api => export_sub2api(accounts),
    }
}

pub fn export_cpa(accounts: &[CodexAuthFile]) -> Value {
    let values = accounts
        .iter()
        .map(CodexAuthFile::to_cpa_value)
        .collect::<Vec<_>>();
    if values.len() == 1 {
        values.into_iter().next().unwrap_or_else(|| json!({}))
    } else {
        Value::Array(values)
    }
}

pub fn export_cpa_zip_from_document(document: &Value) -> Option<Vec<u8>> {
    let accounts = document.as_array()?;
    (accounts.len() > 1).then(|| {
        let entries = accounts
            .iter()
            .enumerate()
            .map(|(index, account)| {
                let filename = cpa_account_filename(index + 1, account);
                let body = serde_json::to_vec_pretty(account).unwrap_or_else(|_| b"{}".to_vec());
                let mut body_with_newline = body;
                body_with_newline.push(b'\n');
                (filename, body_with_newline)
            })
            .collect::<Vec<_>>();
        zip_store_entries(&entries)
    })
}

fn cpa_account_filename(index: usize, account: &Value) -> String {
    let label = ["email", "account_id", "chatgpt_account_id", "name"]
        .iter()
        .find_map(|key| {
            account
                .get(*key)
                .and_then(Value::as_str)
                .map(sanitize_filename_part)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "codex-account".to_string());
    format!("{index:03}-{label}.json")
}

fn sanitize_filename_part(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(['.', '-', '_'])
        .chars()
        .take(80)
        .collect()
}

fn zip_store_entries(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    struct CentralEntry {
        name: Vec<u8>,
        crc32: u32,
        size: u32,
        offset: u32,
    }

    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let name = name.as_bytes().to_vec();
        let offset = out.len() as u32;
        let crc32 = crc32(data);
        let size = data.len() as u32;

        push_u32(&mut out, 0x0403_4b50);
        push_u16(&mut out, 20);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u32(&mut out, crc32);
        push_u32(&mut out, size);
        push_u32(&mut out, size);
        push_u16(&mut out, name.len() as u16);
        push_u16(&mut out, 0);
        out.extend_from_slice(&name);
        out.extend_from_slice(data);

        central.push(CentralEntry {
            name,
            crc32,
            size,
            offset,
        });
    }

    let central_offset = out.len() as u32;
    for entry in &central {
        push_u32(&mut out, 0x0201_4b50);
        push_u16(&mut out, 20);
        push_u16(&mut out, 20);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u32(&mut out, entry.crc32);
        push_u32(&mut out, entry.size);
        push_u32(&mut out, entry.size);
        push_u16(&mut out, entry.name.len() as u16);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u32(&mut out, 0);
        push_u32(&mut out, entry.offset);
        out.extend_from_slice(&entry.name);
    }
    let central_size = out.len() as u32 - central_offset;

    push_u32(&mut out, 0x0605_4b50);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);
    push_u16(&mut out, central.len() as u16);
    push_u16(&mut out, central.len() as u16);
    push_u32(&mut out, central_size);
    push_u32(&mut out, central_offset);
    push_u16(&mut out, 0);
    out
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

pub fn export_sub2api(accounts: &[CodexAuthFile]) -> Value {
    json!({
        "exported_at": Utc::now().to_rfc3339(),
        "proxies": [],
        "accounts": accounts.iter().map(sub2api_account).collect::<Vec<_>>()
    })
}

fn sub2api_account(auth_file: &CodexAuthFile) -> Value {
    let auth_file = auth_file.clone().normalized();
    let expires_at = auth_file.expires_at_epoch();
    json!({
        "name": auth_file.name.clone().or(auth_file.email.clone()).unwrap_or_else(|| "Codex Account".to_string()),
        "platform": "openai",
        "type": "oauth",
        "expires_at": expires_at,
        "auto_pause_on_expired": true,
        "concurrency": 10,
        "priority": 1,
        "credentials": compact_object(json!({
            "access_token": auth_file.access_token,
            "refresh_token": auth_file.refresh_token,
            "id_token": auth_file.id_token,
            "session_token": auth_file.session_token,
            "chatgpt_account_id": auth_file.chatgpt_account_id.or(auth_file.account_id.clone()),
            "email": auth_file.email,
            "expires_at": expires_at,
            "plan_type": auth_file.plan_type,
        })),
        "extra": compact_object(json!({
            "email": auth_file.email,
            "name": auth_file.name,
            "source": "aether_pool_redeem_export",
            "last_refresh": auth_file.last_refresh,
        }))
    })
}

pub fn compact_object(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .filter_map(|(key, value)| match value {
                    Value::Null => None,
                    Value::String(ref text) if text.is_empty() => None,
                    other => Some((key, other)),
                })
                .collect(),
        ),
        other => other,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthCheckResult {
    pub status: AccountStatus,
    pub plan_type: Option<String>,
    pub quota_snapshot: Option<Value>,
    pub error: Option<String>,
}

pub fn normalize_wham_usage_response(status_code: u16, body: Option<Value>) -> HealthCheckResult {
    let error_message = body.as_ref().and_then(extract_upstream_error_message);
    if status_code == 401
        || (status_code == 403 && codex_looks_like_token_invalidated(error_message.as_deref()))
        || (status_code == 402 && codex_looks_like_workspace_deactivated(error_message.as_deref()))
    {
        return HealthCheckResult {
            status: AccountStatus::AuthInvalid,
            plan_type: None,
            quota_snapshot: body,
            error: Some(wham_error_message(status_code, error_message.as_deref())),
        };
    }
    if status_code == 403 {
        return HealthCheckResult {
            status: AccountStatus::Forbidden,
            plan_type: body.as_ref().and_then(extract_plan_type),
            quota_snapshot: body,
            error: Some(wham_error_message(status_code, error_message.as_deref())),
        };
    }
    if status_code == 402 {
        return HealthCheckResult {
            status: AccountStatus::QuotaExhausted,
            plan_type: body.as_ref().and_then(extract_plan_type),
            quota_snapshot: body,
            error: Some(wham_error_message(status_code, error_message.as_deref())),
        };
    }
    if !(200..300).contains(&status_code) {
        return HealthCheckResult {
            status: AccountStatus::RefreshFailed,
            plan_type: None,
            quota_snapshot: body,
            error: Some(wham_error_message(status_code, error_message.as_deref())),
        };
    }

    let now = unix_now_secs();
    let parsed_quota = body
        .as_ref()
        .and_then(|value| parse_codex_wham_usage_response(value, now));
    let plan_type = parsed_quota
        .as_ref()
        .and_then(extract_plan_type)
        .or_else(|| body.as_ref().and_then(extract_plan_type));
    let quota_exhausted = parsed_quota
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(codex_quota_bucket_exhausted);
    HealthCheckResult {
        status: if quota_exhausted {
            AccountStatus::QuotaExhausted
        } else {
            AccountStatus::Available
        },
        plan_type,
        quota_snapshot: parsed_quota.or(body),
        error: None,
    }
}

fn wham_error_message(status_code: u16, detail: Option<&str>) -> String {
    match detail.map(str::trim).filter(|value| !value.is_empty()) {
        Some(detail) => format!("wham/usage returned {status_code}: {detail}"),
        None => format!("wham/usage returned {status_code}"),
    }
}

fn extract_upstream_error_message(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.trim().to_string()).filter(|value| !value.is_empty()),
        Value::Object(object) => {
            for key in ["message", "detail", "reason", "error_description"] {
                if let Some(message) = object.get(key).and_then(extract_upstream_error_message) {
                    return Some(message);
                }
            }
            match object.get("error") {
                Some(Value::String(message)) => {
                    Some(message.trim().to_string()).filter(|value| !value.is_empty())
                }
                Some(Value::Object(_)) | Some(Value::Array(_)) => {
                    object.get("error").and_then(extract_upstream_error_message)
                }
                _ => None,
            }
        }
        Value::Array(items) => items.iter().find_map(extract_upstream_error_message),
        _ => None,
    }
}

fn codex_looks_like_token_invalidated(message: Option<&str>) -> bool {
    let lowered = message.unwrap_or_default().trim().to_ascii_lowercase();
    lowered.contains("token invalid")
        || lowered.contains("token invalidated")
        || lowered.contains("session has expired")
        || lowered.contains("session expired")
        || lowered.contains("account has been deactivated")
        || lowered.contains("account deactivated")
}

fn codex_looks_like_workspace_deactivated(message: Option<&str>) -> bool {
    let lowered = message.unwrap_or_default().trim().to_ascii_lowercase();
    lowered.contains("deactivated_workspace")
        || (lowered.contains("workspace") && lowered.contains("deactivated"))
}

fn extract_plan_type(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => string_field(
            object,
            &[
                "plan_type",
                "chatgpt_plan_type",
                "plan",
                "subscription_plan",
                "account_plan",
            ],
        )
        .or_else(|| object.values().find_map(extract_plan_type)),
        Value::Array(items) => items.iter().find_map(extract_plan_type),
        _ => None,
    }
}

fn parse_codex_wham_usage_response(value: &Value, updated_at_unix_secs: u64) -> Option<Value> {
    let root = value.as_object()?;
    if root.is_empty() {
        return None;
    }

    let mut result = Map::new();
    if let Some(plan_type) = root
        .get("plan_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
    {
        result.insert("plan_type".to_string(), json!(plan_type));
    }

    let rate_limit = root
        .get("rate_limit")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let primary_window = rate_limit
        .get("primary_window")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let secondary_window = rate_limit
        .get("secondary_window")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let use_paid_windows = !secondary_window.is_empty()
        && result.get("plan_type").and_then(Value::as_str) != Some("free");
    if use_paid_windows {
        codex_write_window(&mut result, &secondary_window, "primary");
        codex_write_window(&mut result, &primary_window, "secondary");
    } else {
        codex_write_window(&mut result, &primary_window, "primary");
    }

    if let Some(credits) = root.get("credits").and_then(Value::as_object) {
        if let Some(value) = credits.get("has_credits").and_then(coerce_json_bool) {
            result.insert("has_credits".to_string(), json!(value));
        }
        if let Some(value) = credits.get("balance").and_then(coerce_json_f64) {
            result.insert("credits_balance".to_string(), json!(value));
        }
        if let Some(value) = credits.get("unlimited").and_then(coerce_json_bool) {
            result.insert("credits_unlimited".to_string(), json!(value));
        }
    }

    if result.is_empty() {
        return None;
    }
    result.insert("updated_at".to_string(), json!(updated_at_unix_secs));
    Some(Value::Object(result))
}

fn codex_write_window(target: &mut Map<String, Value>, source: &Map<String, Value>, prefix: &str) {
    if let Some(value) = source.get("used_percent").and_then(coerce_json_f64) {
        target.insert(format!("{prefix}_used_percent"), json!(value));
    }
    if let Some(value) = source.get("reset_after_seconds").and_then(coerce_json_u64) {
        target.insert(format!("{prefix}_reset_after_seconds"), json!(value));
    }
    if let Some(value) = source.get("reset_at").and_then(coerce_json_u64) {
        target.insert(format!("{prefix}_reset_at"), json!(value));
    }
    if let Some(value) = source.get("window_minutes").and_then(coerce_json_u64) {
        target.insert(format!("{prefix}_window_minutes"), json!(value));
    }
    if let Some(value) = source
        .get("limit_window_seconds")
        .and_then(coerce_json_u64)
        .map(|seconds| seconds / 60)
    {
        target.insert(format!("{prefix}_window_minutes"), json!(value));
    }
}

fn codex_quota_bucket_exhausted(bucket: &Map<String, Value>) -> bool {
    if bucket.get("credits_unlimited").and_then(coerce_json_bool) == Some(true) {
        return false;
    }
    let has_window_data = bucket
        .get("primary_used_percent")
        .and_then(coerce_json_f64)
        .is_some()
        || bucket
            .get("secondary_used_percent")
            .and_then(coerce_json_f64)
            .is_some();
    if !has_window_data && bucket.get("has_credits").and_then(coerce_json_bool) == Some(false) {
        return true;
    }
    codex_window_used_percent_exhausted(bucket, "primary")
        || codex_window_used_percent_exhausted(bucket, "secondary")
}

fn codex_window_used_percent_exhausted(bucket: &Map<String, Value>, prefix: &str) -> bool {
    let used_percent_key = format!("{prefix}_used_percent");
    bucket
        .get(used_percent_key.as_str())
        .and_then(coerce_json_f64)
        .is_some_and(|value| value >= 100.0 && !codex_window_reset_elapsed(bucket, prefix))
}

fn codex_window_reset_elapsed(bucket: &Map<String, Value>, prefix: &str) -> bool {
    let Some(updated_at) = bucket.get("updated_at").and_then(coerce_json_u64) else {
        return false;
    };
    let now = unix_now_secs();
    let reset_at_key = format!("{prefix}_reset_at");
    if let Some(reset_at) = bucket.get(reset_at_key.as_str()).and_then(coerce_json_u64) {
        return reset_at <= now;
    }
    let reset_after_key = format!("{prefix}_reset_after_seconds");
    bucket
        .get(reset_after_key.as_str())
        .and_then(coerce_json_u64)
        .is_some_and(|seconds| updated_at.saturating_add(seconds) <= now)
}

fn coerce_json_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::Number(number) => number.as_u64().map(|value| value != 0),
        Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn coerce_json_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => raw.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn coerce_json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().map(normalize_epoch_seconds),
        Value::String(raw) => raw.trim().parse::<u64>().ok().map(normalize_epoch_seconds),
        _ => None,
    }
}

pub fn normalize_redeem_code(value: &str) -> Option<String> {
    let clean = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_uppercase())
        .collect::<String>();
    (12..=64).contains(&clean.len()).then_some(clean)
}

pub fn format_redeem_code(normalized: &str) -> String {
    normalized
        .as_bytes()
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn generate_redeem_code() -> String {
    let raw = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(char::from)
        .map(|ch| ch.to_ascii_uppercase())
        .take(16)
        .collect::<String>();
    format_redeem_code(&raw)
}

pub fn redeem_code_hash(normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn mask_redeem_code(normalized: &str) -> String {
    let prefix = normalized.chars().take(4).collect::<String>();
    let suffix = normalized
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{prefix}-****-****-{suffix}")
}

pub fn unix_now_secs() -> u64 {
    Utc::now().timestamp().max(0) as u64
}

fn decode_base64_url_part(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value.as_bytes()).ok()
}

fn decode_access_token_claims(access_token: &str) -> Option<Map<String, Value>> {
    let payload = access_token.trim().split('.').nth(1)?;
    let bytes = decode_base64_url_part(payload)?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .as_object()
        .cloned()
}

pub fn decode_access_token_expires_at(access_token: &str) -> Option<u64> {
    decode_access_token_claims(access_token)?
        .get("exp")
        .and_then(json_u64_value)
}

pub fn looks_like_access_token(token: &str) -> bool {
    let parts = token.trim().split('.').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return false;
    }
    decode_access_token_claims(token).is_some_and(|claims| {
        ["exp", "aud", "iss", "scope", "scp"]
            .iter()
            .any(|field| claims.contains_key(*field))
    })
}

fn json_u64_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(raw) => raw.trim().parse::<u64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsigned_jwt(payload: Value) -> String {
        let header = json!({"alg":"none","typ":"JWT"});
        let encode = |value: Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap());
        format!("{}.{}.sig", encode(header), encode(payload))
    }

    #[test]
    fn redeemed_deletable_statuses_exclude_quota_exhausted() {
        assert!(is_redeemed_account_deletable_status("auth_invalid"));
        assert!(is_redeemed_account_deletable_status("refresh_failed"));
        assert!(is_redeemed_account_deletable_status("at_expired"));
        assert!(is_redeemed_account_deletable_status("forbidden"));
        assert!(!is_redeemed_account_deletable_status("available"));
        assert!(!is_redeemed_account_deletable_status("quota_exhausted"));
    }

    #[test]
    fn parses_cpa_auth_file() {
        let token = unsigned_jwt(json!({
            "exp": 2_000_000_000u64,
            "https://api.openai.com/profile": {"email": "u@example.com"}
        }));
        let input = json!({"access_token": token, "refresh_token": "rt", "plan_type": "plus"});
        let parsed = parse_codex_accounts(&input.to_string());
        assert_eq!(parsed.errors, Vec::<String>::new());
        assert_eq!(parsed.accounts.len(), 1);
        assert_eq!(
            parsed.accounts[0].auth_file.email.as_deref(),
            Some("u@example.com")
        );
        assert_eq!(
            parsed.accounts[0].auth_file.expires_at_epoch(),
            Some(2_000_000_000)
        );
    }

    #[test]
    fn parses_sub2api_accounts() {
        let input = json!({
            "accounts": [{
                "name": "demo",
                "credentials": {
                    "access_token": "at",
                    "refresh_token": "rt",
                    "email": "demo@example.com"
                }
            }]
        });
        let parsed = parse_codex_accounts(&input.to_string());
        assert_eq!(parsed.accounts.len(), 1);
        assert_eq!(parsed.accounts[0].source, "sub2api");
    }

    #[test]
    fn fingerprints_distinguish_same_workspace_accounts_by_email() {
        let first = CodexAuthFile {
            account_id: Some("workspace-1".to_string()),
            chatgpt_account_id: Some("workspace-1".to_string()),
            email: Some("User-A@example.com".to_string()),
            access_token: Some("access-a".to_string()),
            refresh_token: Some("refresh-a".to_string()),
            ..CodexAuthFile::default()
        }
        .normalized();
        let first_updated = CodexAuthFile {
            account_id: Some("workspace-1".to_string()),
            chatgpt_account_id: Some("workspace-1".to_string()),
            email: Some("user-a@example.com".to_string()),
            access_token: Some("access-a-updated".to_string()),
            refresh_token: Some("refresh-a-updated".to_string()),
            ..CodexAuthFile::default()
        }
        .normalized();
        let second = CodexAuthFile {
            account_id: Some("workspace-1".to_string()),
            chatgpt_account_id: Some("workspace-1".to_string()),
            email: Some("user-b@example.com".to_string()),
            access_token: Some("access-b".to_string()),
            refresh_token: Some("refresh-b".to_string()),
            ..CodexAuthFile::default()
        }
        .normalized();

        assert_eq!(
            fingerprint_auth_file(&first),
            fingerprint_auth_file(&first_updated)
        );
        assert_ne!(
            fingerprint_auth_file(&first),
            fingerprint_auth_file(&second)
        );
        assert_eq!(
            legacy_fingerprint_auth_file(&first),
            legacy_fingerprint_auth_file(&second)
        );
    }

    #[test]
    fn detects_refresh_need() {
        assert!(access_token_needs_refresh(Some(1_000), 900, 120));
        assert!(!access_token_needs_refresh(Some(2_000), 900, 120));
        assert!(!access_token_needs_refresh(None, 900, 120));
    }

    #[test]
    fn exports_sub2api_shape() {
        let auth = CodexAuthFile {
            email: Some("a@example.com".to_string()),
            access_token: Some("at".to_string()),
            refresh_token: Some("rt".to_string()),
            ..CodexAuthFile::default()
        };
        let exported = export_accounts(ExportFormat::Sub2api, &[auth]);
        assert!(
            exported
                .get("accounts")
                .and_then(Value::as_array)
                .unwrap()
                .len()
                == 1
        );
    }

    #[test]
    fn exports_multi_cpa_as_zip_entries() {
        let document = export_accounts(
            ExportFormat::Cpa,
            &[
                CodexAuthFile {
                    email: Some("a@example.com".to_string()),
                    access_token: Some("at-a".to_string()),
                    ..CodexAuthFile::default()
                },
                CodexAuthFile {
                    account_id: Some("acct/b".to_string()),
                    access_token: Some("at-b".to_string()),
                    ..CodexAuthFile::default()
                },
            ],
        );
        let archive = export_cpa_zip_from_document(&document).unwrap();
        assert!(archive.starts_with(b"PK\x03\x04"));
        let archive_text = String::from_utf8_lossy(&archive);
        assert!(archive_text.contains("001-a-example.com.json"));
        assert!(archive_text.contains("002-acct-b.json"));
        assert!(archive_text.contains("\"access_token\": \"at-a\""));
        assert!(archive_text.contains("\"access_token\": \"at-b\""));
    }

    #[test]
    fn wham_usage_402_is_quota_exhausted() {
        let result = normalize_wham_usage_response(
            402,
            Some(json!({
                "plan_type": "plus",
                "error": {"message": "quota exceeded"}
            })),
        );

        assert_eq!(result.status, AccountStatus::QuotaExhausted);
        assert_eq!(result.plan_type.as_deref(), Some("plus"));
    }

    #[test]
    fn wham_usage_credits_not_unlimited_does_not_mean_exhausted() {
        let result = normalize_wham_usage_response(
            200,
            Some(json!({
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 25.0,
                        "reset_after_seconds": 604800
                    },
                    "secondary_window": {
                        "used_percent": 10.0,
                        "reset_after_seconds": 18000
                    }
                },
                "credits": {
                    "has_credits": true,
                    "balance": 12.5,
                    "unlimited": false
                }
            })),
        );

        assert_eq!(result.status, AccountStatus::Available);
        assert_eq!(result.plan_type.as_deref(), Some("plus"));
        let snapshot = result.quota_snapshot.unwrap();
        assert_eq!(snapshot.get("primary_used_percent"), Some(&json!(10.0)));
        assert_eq!(snapshot.get("secondary_used_percent"), Some(&json!(25.0)));
        assert_eq!(snapshot.get("credits_unlimited"), Some(&json!(false)));
    }

    #[test]
    fn wham_usage_window_at_one_hundred_is_exhausted_until_reset() {
        let future_reset = unix_now_secs().saturating_add(3600);
        let result = normalize_wham_usage_response(
            200,
            Some(json!({
                "plan_type": "free",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 100.0,
                        "reset_at": future_reset
                    }
                }
            })),
        );

        assert_eq!(result.status, AccountStatus::QuotaExhausted);
    }

    #[test]
    fn wham_usage_elapsed_reset_window_is_available() {
        let result = normalize_wham_usage_response(
            200,
            Some(json!({
                "plan_type": "free",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 100.0,
                        "reset_at": 1
                    }
                }
            })),
        );

        assert_eq!(result.status, AccountStatus::Available);
    }

    #[test]
    fn wham_usage_no_windows_without_credits_is_exhausted() {
        let result = normalize_wham_usage_response(
            200,
            Some(json!({
                "plan_type": "plus",
                "credits": {
                    "has_credits": false,
                    "unlimited": false
                }
            })),
        );

        assert_eq!(result.status, AccountStatus::QuotaExhausted);
    }

    #[test]
    fn wham_usage_plain_403_is_forbidden_not_auth_invalid() {
        let result = normalize_wham_usage_response(
            403,
            Some(json!({
                "error": {"message": "forbidden"}
            })),
        );

        assert_eq!(result.status, AccountStatus::Forbidden);
    }

    #[test]
    fn wham_usage_token_invalid_403_is_auth_invalid() {
        let result = normalize_wham_usage_response(
            403,
            Some(json!({
                "error": {"message": "Token invalidated"}
            })),
        );

        assert_eq!(result.status, AccountStatus::AuthInvalid);
    }

    #[test]
    fn normalizes_redeem_codes() {
        let code = generate_redeem_code();
        let normalized = normalize_redeem_code(&code).unwrap();
        assert_eq!(normalized.len(), 16);
        assert!(mask_redeem_code(&normalized).contains("****"));
        assert!(normalize_redeem_code(&"A".repeat(65)).is_none());
    }
}
