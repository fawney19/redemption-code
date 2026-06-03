CREATE TABLE IF NOT EXISTS accounts (
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
);

CREATE INDEX IF NOT EXISTS idx_accounts_status ON accounts(status, updated_at);
CREATE INDEX IF NOT EXISTS idx_accounts_redeemed_at ON accounts(redeemed_at);
CREATE INDEX IF NOT EXISTS idx_accounts_plan_type ON accounts(plan_type);

CREATE TABLE IF NOT EXISTS account_health_checks (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL,
  status TEXT NOT NULL,
  http_status INTEGER,
  latency_ms INTEGER,
  quota_snapshot TEXT,
  error TEXT,
  created_at INTEGER NOT NULL,
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_account_health_checks_account ON account_health_checks(account_id, created_at);

CREATE TABLE IF NOT EXISTS account_exports (
  id TEXT PRIMARY KEY,
  format TEXT NOT NULL,
  source TEXT NOT NULL,
  account_ids_json TEXT NOT NULL,
  account_count INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS redeem_code_batches (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'active',
  total_count INTEGER NOT NULL,
  redeemed_count INTEGER NOT NULL DEFAULT 0,
  accounts_per_code INTEGER NOT NULL,
  plan_filter_json TEXT,
  expires_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_redeem_code_batches_status ON redeem_code_batches(status, created_at);

CREATE TABLE IF NOT EXISTS redeem_codes (
  id TEXT PRIMARY KEY,
  batch_id TEXT NOT NULL,
  code_hash TEXT NOT NULL UNIQUE,
  code_prefix TEXT NOT NULL,
  code_suffix TEXT NOT NULL,
  masked_code TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'active',
  redemption_id TEXT,
  redeemed_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  FOREIGN KEY(batch_id) REFERENCES redeem_code_batches(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_redeem_codes_batch ON redeem_codes(batch_id, created_at);
CREATE INDEX IF NOT EXISTS idx_redeem_codes_status ON redeem_codes(status, updated_at);

CREATE TABLE IF NOT EXISTS redeem_redemptions (
  id TEXT PRIMARY KEY,
  code_id TEXT NOT NULL,
  batch_id TEXT NOT NULL,
  export_format TEXT NOT NULL,
  account_ids_json TEXT NOT NULL,
  export_snapshot_ciphertext TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  FOREIGN KEY(code_id) REFERENCES redeem_codes(id) ON DELETE CASCADE,
  FOREIGN KEY(batch_id) REFERENCES redeem_code_batches(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_redeem_redemptions_code ON redeem_redemptions(code_id, created_at);
