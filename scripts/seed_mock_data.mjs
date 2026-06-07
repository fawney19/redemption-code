#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import crypto from 'node:crypto'
import { existsSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const dbPath = resolve(repoRoot, process.env.AETHER_POOL_MOCK_DB_PATH || 'data/aether-pool.sqlite3')
const secretKey = process.env.AETHER_POOL_SECRET_KEY || 'dev-only-secret-key'

if (!existsSync(dbPath)) {
  console.error(`SQLite database not found: ${dbPath}`)
  process.exit(1)
}

const now = Math.floor(Date.now() / 1000)
const hour = 60 * 60
const day = 24 * hour
const cipherKey = crypto.createHash('sha256').update(secretKey.trim() || 'aether-pool-local-development-secret').digest()

function b64NoPad(buffer) {
  return buffer.toString('base64').replace(/=+$/g, '')
}

function encryptJson(value) {
  const nonce = crypto.randomBytes(12)
  const cipher = crypto.createCipheriv('aes-256-gcm', cipherKey, nonce)
  const body = Buffer.concat([
    cipher.update(Buffer.from(JSON.stringify(value), 'utf8')),
    cipher.final(),
    cipher.getAuthTag(),
  ])
  return `v1:${b64NoPad(nonce)}:${b64NoPad(body)}`
}

function sql(value) {
  if (value === null || value === undefined) return 'NULL'
  if (typeof value === 'number') return Number.isFinite(value) ? String(Math.trunc(value)) : 'NULL'
  if (typeof value === 'boolean') return value ? '1' : '0'
  return `'${String(value).replace(/'/g, "''")}'`
}

function insert(table, row) {
  const columns = Object.keys(row)
  const values = columns.map((column) => sql(row[column]))
  return `INSERT INTO ${table} (${columns.join(', ')}) VALUES (${values.join(', ')});`
}

function sha256Hex(value) {
  return crypto.createHash('sha256').update(value).digest('hex')
}

function preview(value) {
  const text = String(value || '').trim()
  if (!text) return null
  if (text.length <= 12) return `${text.slice(0, Math.min(text.length, 4))}...`
  return `${text.slice(0, 6)}...${text.slice(-4)}`
}

function b64Url(value) {
  return Buffer.from(JSON.stringify(value)).toString('base64url')
}

function mockJwt(accountId, email, expiresAt) {
  const header = { alg: 'none', typ: 'JWT' }
  const payload = {
    sub: accountId,
    email,
    exp: expiresAt,
    aud: 'mock-codex',
    scope: 'openid profile email',
    'https://api.openai.com/profile': {
      email,
      user_id: accountId,
    },
    'https://api.openai.com/auth': {
      chatgpt_account_id: accountId,
    },
  }
  return `${b64Url(header)}.${b64Url(payload)}.mock-signature`
}

function quota(primary, secondary) {
  return JSON.stringify({
    primary_used_percent: primary,
    secondary_used_percent: secondary,
    primary_window: '5h',
    secondary_window: 'weekly',
    mock: true,
  })
}

function statusProbe(status) {
  if (status === 'available' || status === 'redeemed') return { http: 200, error: null }
  if (status === 'quota_exhausted') return { http: 200, error: 'quota exhausted' }
  if (status === 'forbidden') return { http: 403, error: 'proxy or region forbidden' }
  if (status === 'refresh_failed') return { http: null, error: 'refresh token rejected' }
  return { http: 401, error: 'access token expired or invalid' }
}

function normalizeCode(value) {
  return value.replace(/[^a-z0-9]/gi, '').toUpperCase()
}

function formatCode(normalized) {
  return normalized.match(/.{1,4}/g).join('-')
}

function maskCode(normalized) {
  return `${normalized.slice(0, 4)}-****-****-${normalized.slice(-4)}`
}

function codeRow(id, batchId, formattedCode, status = 'active', redemptionId = null, redeemedAt = null, createdOffset = 0) {
  const normalized = normalizeCode(formattedCode)
  return {
    id,
    batch_id: batchId,
    code_hash: sha256Hex(normalized),
    code_prefix: normalized.slice(0, 4),
    code_suffix: normalized.slice(-4),
    masked_code: maskCode(normalized),
    code_ciphertext: encryptJson(formatCode(normalized)),
    status,
    redemption_id: redemptionId,
    redeemed_at: redeemedAt,
    created_at: now - createdOffset,
    updated_at: now - Math.min(createdOffset, hour),
  }
}

function makeAccount(input) {
  const accountId = input.accountId || input.id.replace(/^mock-account-/, 'acct-mock-')
  const expiresAt = input.expiresAt ?? now + 21 * day
  const accessToken = mockJwt(accountId, input.email, expiresAt)
  const refreshToken = `rt_mock_${accountId.replace(/[^a-z0-9]/gi, '_')}_${sha256Hex(input.email).slice(0, 16)}`
  const authFile = {
    type: 'codex',
    account_id: accountId,
    chatgpt_account_id: accountId,
    email: input.email,
    name: input.name,
    plan_type: input.plan,
    chatgpt_plan_type: input.plan,
    access_token: accessToken,
    refresh_token: refreshToken,
    expires_at: expiresAt,
    last_refresh: new Date((input.lastRefreshAt ?? now - 6 * hour) * 1000).toISOString(),
    mock: true,
  }
  return {
    authFile,
    row: {
      id: input.id,
      pool_id: input.poolId,
      email: input.email,
      name: input.name,
      account_id: accountId,
      plan_type: input.plan,
      status: input.status,
      auth_fingerprint: sha256Hex(`email:${input.email.toLowerCase()}|workspace:${accountId}`),
      auth_file_ciphertext: encryptJson(authFile),
      access_token_preview: preview(accessToken),
      refresh_token_preview: preview(refreshToken),
      expires_at: expiresAt,
      last_refresh_at: input.lastRefreshAt ?? now - 6 * hour,
      last_probe_at: input.lastProbeAt ?? now - input.probeAge,
      quota_snapshot: quota(input.primaryQuota, input.weeklyQuota),
      redeem_code_id: input.redeemCodeId ?? null,
      redemption_id: input.redemptionId ?? null,
      redeemed_at: input.redeemedAt ?? null,
      redeem_reservation_id: null,
      redeem_reserved_at: null,
      created_at: input.createdAt ?? now - input.age,
      updated_at: input.updatedAt ?? now - Math.min(input.age, day),
    },
  }
}

const pools = [
  {
    id: 'mock-pool-pro',
    name: 'Mock Pro 号池',
    workspace_label: 'Mock Pro Workspace',
    account_type: 'codex',
    description: '用于演示 Pro / Team 账号、售后补发和高额度状态',
    is_default: 0,
    is_active: 1,
    created_at: now - 15 * day,
    updated_at: now - hour,
  },
  {
    id: 'mock-pool-trial',
    name: 'Mock Trial 号池',
    workspace_label: 'Mock Trial Workspace',
    account_type: 'codex',
    description: '用于演示试用账号和过期兑换码批次',
    is_default: 0,
    is_active: 1,
    created_at: now - 12 * day,
    updated_at: now - 2 * hour,
  },
]

const redemptions = {
  default: {
    id: 'mock-redemption-default-01',
    codeId: 'mock-code-main-01',
    batchId: 'mock-batch-main',
    accountIds: ['mock-account-default-02'],
    createdAt: now - 20 * hour,
  },
  proOriginal: {
    id: 'mock-redemption-pro-original-01',
    codeId: 'mock-code-pro-01',
    batchId: 'mock-batch-pro',
    accountIds: ['mock-account-pro-01', 'mock-account-pro-02'],
    createdAt: now - 3 * day,
  },
  proReplacement: {
    id: 'mock-redemption-pro-replacement-01',
    codeId: 'mock-code-pro-01',
    batchId: 'mock-batch-pro',
    accountIds: ['mock-account-pro-03', 'mock-account-pro-04'],
    createdAt: now - 8 * hour,
  },
}

const accounts = [
  makeAccount({
    id: 'mock-account-default-01',
    poolId: 'default',
    email: 'mock.available.01@example.com',
    name: 'Mock Available 01',
    plan: 'plus',
    status: 'available',
    primaryQuota: 18,
    weeklyQuota: 31,
    probeAge: 14 * 60,
    age: 9 * day,
  }),
  makeAccount({
    id: 'mock-account-default-02',
    poolId: 'default',
    email: 'mock.redeemed.01@example.com',
    name: 'Mock Redeemed 01',
    plan: 'plus',
    status: 'available',
    primaryQuota: 42,
    weeklyQuota: 58,
    probeAge: 22 * 60,
    age: 8 * day,
    redeemCodeId: redemptions.default.codeId,
    redemptionId: redemptions.default.id,
    redeemedAt: redemptions.default.createdAt,
  }),
  makeAccount({
    id: 'mock-account-default-03',
    poolId: 'default',
    email: 'mock.expired.01@example.com',
    name: 'Mock AT Expired 01',
    plan: 'plus',
    status: 'at_expired',
    primaryQuota: 66,
    weeklyQuota: 74,
    probeAge: 45 * 60,
    age: 11 * day,
    expiresAt: now - 2 * hour,
  }),
  makeAccount({
    id: 'mock-account-default-04',
    poolId: 'default',
    email: 'mock.refresh.failed.01@example.com',
    name: 'Mock Refresh Failed 01',
    plan: 'team',
    status: 'refresh_failed',
    primaryQuota: 71,
    weeklyQuota: 40,
    probeAge: 2 * hour,
    age: 10 * day,
  }),
  makeAccount({
    id: 'mock-account-default-05',
    poolId: 'default',
    email: 'mock.quota.01@example.com',
    name: 'Mock Quota Exhausted 01',
    plan: 'plus',
    status: 'quota_exhausted',
    primaryQuota: 100,
    weeklyQuota: 96,
    probeAge: 25 * 60,
    age: 6 * day,
  }),
  makeAccount({
    id: 'mock-account-default-06',
    poolId: 'default',
    email: 'mock.forbidden.01@example.com',
    name: 'Mock Forbidden 01',
    plan: 'plus',
    status: 'forbidden',
    primaryQuota: 29,
    weeklyQuota: 44,
    probeAge: 75 * 60,
    age: 5 * day,
  }),
  makeAccount({
    id: 'mock-account-pro-01',
    poolId: 'mock-pool-pro',
    email: 'mock.pro.old.01@example.com',
    name: 'Mock Pro Old 01',
    plan: 'pro',
    status: 'refresh_failed',
    primaryQuota: 83,
    weeklyQuota: 69,
    probeAge: 3 * hour,
    age: 14 * day,
    redeemCodeId: redemptions.proOriginal.codeId,
    redemptionId: redemptions.proOriginal.id,
    redeemedAt: redemptions.proOriginal.createdAt,
  }),
  makeAccount({
    id: 'mock-account-pro-02',
    poolId: 'mock-pool-pro',
    email: 'mock.pro.old.02@example.com',
    name: 'Mock Pro Old 02',
    plan: 'team',
    status: 'at_expired',
    primaryQuota: 91,
    weeklyQuota: 82,
    probeAge: 4 * hour,
    age: 13 * day,
    expiresAt: now - hour,
    redeemCodeId: redemptions.proOriginal.codeId,
    redemptionId: redemptions.proOriginal.id,
    redeemedAt: redemptions.proOriginal.createdAt,
  }),
  makeAccount({
    id: 'mock-account-pro-03',
    poolId: 'mock-pool-pro',
    email: 'mock.pro.replacement.01@example.com',
    name: 'Mock Pro Replacement 01',
    plan: 'pro',
    status: 'available',
    primaryQuota: 12,
    weeklyQuota: 24,
    probeAge: 9 * 60,
    age: 7 * day,
    redeemCodeId: redemptions.proReplacement.codeId,
    redemptionId: redemptions.proReplacement.id,
    redeemedAt: redemptions.proReplacement.createdAt,
  }),
  makeAccount({
    id: 'mock-account-pro-04',
    poolId: 'mock-pool-pro',
    email: 'mock.pro.replacement.02@example.com',
    name: 'Mock Pro Replacement 02',
    plan: 'team',
    status: 'available',
    primaryQuota: 21,
    weeklyQuota: 35,
    probeAge: 12 * 60,
    age: 7 * day,
    redeemCodeId: redemptions.proReplacement.codeId,
    redemptionId: redemptions.proReplacement.id,
    redeemedAt: redemptions.proReplacement.createdAt,
  }),
  makeAccount({
    id: 'mock-account-pro-05',
    poolId: 'mock-pool-pro',
    email: 'mock.pro.available.01@example.com',
    name: 'Mock Pro Available 01',
    plan: 'pro',
    status: 'available',
    primaryQuota: 8,
    weeklyQuota: 19,
    probeAge: 18 * 60,
    age: 6 * day,
  }),
  makeAccount({
    id: 'mock-account-pro-06',
    poolId: 'mock-pool-pro',
    email: 'mock.team.available.01@example.com',
    name: 'Mock Team Available 01',
    plan: 'team',
    status: 'available',
    primaryQuota: 36,
    weeklyQuota: 47,
    probeAge: 28 * 60,
    age: 5 * day,
  }),
  makeAccount({
    id: 'mock-account-pro-07',
    poolId: 'mock-pool-pro',
    email: 'mock.pro.available.02@example.com',
    name: 'Mock Pro Available 02',
    plan: 'pro',
    status: 'available',
    primaryQuota: 64,
    weeklyQuota: 51,
    probeAge: 38 * 60,
    age: 4 * day,
  }),
  makeAccount({
    id: 'mock-account-pro-08',
    poolId: 'mock-pool-pro',
    email: 'mock.pro.quota.01@example.com',
    name: 'Mock Pro Quota 01',
    plan: 'pro',
    status: 'quota_exhausted',
    primaryQuota: 100,
    weeklyQuota: 100,
    probeAge: 55 * 60,
    age: 4 * day,
  }),
  makeAccount({
    id: 'mock-account-trial-01',
    poolId: 'mock-pool-trial',
    email: 'mock.trial.available.01@example.com',
    name: 'Mock Trial Available 01',
    plan: 'free',
    status: 'available',
    primaryQuota: 4,
    weeklyQuota: 9,
    probeAge: 16 * 60,
    age: 4 * day,
  }),
  makeAccount({
    id: 'mock-account-trial-02',
    poolId: 'mock-pool-trial',
    email: 'mock.trial.available.02@example.com',
    name: 'Mock Trial Available 02',
    plan: 'free',
    status: 'available',
    primaryQuota: 57,
    weeklyQuota: 22,
    probeAge: 33 * 60,
    age: 3 * day,
  }),
  makeAccount({
    id: 'mock-account-trial-03',
    poolId: 'mock-pool-trial',
    email: 'mock.trial.invalid.01@example.com',
    name: 'Mock Trial Invalid 01',
    plan: 'free',
    status: 'auth_invalid',
    primaryQuota: 0,
    weeklyQuota: 0,
    probeAge: 5 * hour,
    age: 2 * day,
    expiresAt: now - day,
  }),
  makeAccount({
    id: 'mock-account-trial-04',
    poolId: 'mock-pool-trial',
    email: 'mock.trial.expired.01@example.com',
    name: 'Mock Trial Expired 01',
    plan: 'free',
    status: 'at_expired',
    primaryQuota: 76,
    weeklyQuota: 64,
    probeAge: 7 * hour,
    age: 2 * day,
    expiresAt: now - 3 * hour,
  }),
]

const authById = new Map(accounts.map((account) => [account.row.id, account.authFile]))

const batches = [
  {
    id: 'mock-batch-main',
    pool_id: 'default',
    name: 'Mock 默认可兑换批次',
    status: 'active',
    total_count: 8,
    redeemed_count: 1,
    accounts_per_code: 1,
    after_sale_limit: 2,
    plan_filter_json: null,
    expires_at: now + 30 * day,
    created_at: now - 2 * day,
    updated_at: now - 20 * hour,
  },
  {
    id: 'mock-batch-pro',
    pool_id: 'mock-pool-pro',
    name: 'Mock Pro 售后批次',
    status: 'active',
    total_count: 5,
    redeemed_count: 1,
    accounts_per_code: 2,
    after_sale_limit: 2,
    plan_filter_json: JSON.stringify(['pro', 'team']),
    expires_at: now + 14 * day,
    created_at: now - 4 * day,
    updated_at: now - 8 * hour,
  },
  {
    id: 'mock-batch-trial-expired',
    pool_id: 'mock-pool-trial',
    name: 'Mock Trial 已过期批次',
    status: 'active',
    total_count: 4,
    redeemed_count: 0,
    accounts_per_code: 1,
    after_sale_limit: 0,
    plan_filter_json: JSON.stringify(['free']),
    expires_at: now - day,
    created_at: now - 10 * day,
    updated_at: now - 2 * day,
  },
]

const codes = [
  codeRow('mock-code-main-01', 'mock-batch-main', 'MOCK-DEMO-2026-0001', 'redeemed', redemptions.default.id, redemptions.default.createdAt, 2 * day),
  codeRow('mock-code-main-02', 'mock-batch-main', 'MOCK-DEMO-2026-0002', 'active', null, null, 2 * day - 60),
  codeRow('mock-code-main-03', 'mock-batch-main', 'MOCK-DEMO-2026-0003', 'active', null, null, 2 * day - 120),
  codeRow('mock-code-main-04', 'mock-batch-main', 'MOCK-DEMO-2026-0004', 'active', null, null, 2 * day - 180),
  codeRow('mock-code-main-05', 'mock-batch-main', 'MOCK-DEMO-2026-0005', 'active', null, null, 2 * day - 240),
  codeRow('mock-code-main-06', 'mock-batch-main', 'MOCK-DEMO-2026-0006', 'active', null, null, 2 * day - 300),
  codeRow('mock-code-main-07', 'mock-batch-main', 'MOCK-DEMO-2026-0007', 'active', null, null, 2 * day - 360),
  codeRow('mock-code-main-08', 'mock-batch-main', 'MOCK-DEMO-2026-0008', 'active', null, null, 2 * day - 420),
  codeRow('mock-code-pro-01', 'mock-batch-pro', 'MOCK-PROA-2026-0001', 'redeemed', redemptions.proReplacement.id, redemptions.proOriginal.createdAt, 4 * day),
  codeRow('mock-code-pro-02', 'mock-batch-pro', 'MOCK-PROA-2026-0002', 'active', null, null, 4 * day - 60),
  codeRow('mock-code-pro-03', 'mock-batch-pro', 'MOCK-PROA-2026-0003', 'active', null, null, 4 * day - 120),
  codeRow('mock-code-pro-04', 'mock-batch-pro', 'MOCK-PROA-2026-0004', 'active', null, null, 4 * day - 180),
  codeRow('mock-code-pro-05', 'mock-batch-pro', 'MOCK-PROA-2026-0005', 'active', null, null, 4 * day - 240),
  codeRow('mock-code-trial-01', 'mock-batch-trial-expired', 'MOCK-FREE-2026-0001', 'active', null, null, 10 * day),
  codeRow('mock-code-trial-02', 'mock-batch-trial-expired', 'MOCK-FREE-2026-0002', 'active', null, null, 10 * day - 60),
  codeRow('mock-code-trial-03', 'mock-batch-trial-expired', 'MOCK-FREE-2026-0003', 'active', null, null, 10 * day - 120),
  codeRow('mock-code-trial-04', 'mock-batch-trial-expired', 'MOCK-FREE-2026-0004', 'active', null, null, 10 * day - 180),
]

const redemptionRows = Object.values(redemptions).map((redemption) => ({
  id: redemption.id,
  code_id: redemption.codeId,
  batch_id: redemption.batchId,
  export_format: 'cpa',
  account_ids_json: JSON.stringify(redemption.accountIds),
  export_snapshot_ciphertext: encryptJson(redemption.accountIds.map((id) => authById.get(id))),
  created_at: redemption.createdAt,
}))

const afterSales = [
  {
    id: 'mock-after-sale-pro-01',
    code_id: redemptions.proOriginal.codeId,
    batch_id: redemptions.proOriginal.batchId,
    original_redemption_id: redemptions.proOriginal.id,
    replacement_redemption_id: redemptions.proReplacement.id,
    old_account_ids_json: JSON.stringify(redemptions.proOriginal.accountIds),
    new_account_ids_json: JSON.stringify(redemptions.proReplacement.accountIds),
    export_format: 'cpa',
    export_snapshot_ciphertext: encryptJson(redemptions.proReplacement.accountIds.map((id) => authById.get(id))),
    status: 'success',
    reason: 'Mock 自动售后补发',
    created_at: redemptions.proReplacement.createdAt,
  },
]

const healthChecks = accounts.map((account, index) => {
  const probe = statusProbe(account.row.status)
  return {
    id: `mock-health-${String(index + 1).padStart(2, '0')}`,
    account_id: account.row.id,
    status: account.row.status,
    http_status: probe.http,
    latency_ms: probe.http ? 120 + index * 17 : null,
    quota_snapshot: account.row.quota_snapshot,
    error: probe.error,
    created_at: account.row.last_probe_at,
  }
})

const settings = [
  {
    key: 'auto_probe',
    value_json: JSON.stringify({
      mock: true,
      enabled: false,
      interval_seconds: 1800,
      max_accounts_per_run: 50,
      concurrency: 6,
      refresh_before_probe: false,
      probe_mode: 'hybrid',
      deep_check_enabled: true,
      cpa_base_url: null,
      cpa_management_key_set: false,
      proxy_enabled: false,
      proxy_mode: 'fixed',
      proxy_url: null,
      proxy_api_url: null,
      proxy_default_scheme: 'http',
      last_started_at: now - hour,
      last_finished_at: now - hour + 96,
      last_checked_count: accounts.length,
      last_error: null,
      last_result: {
        success: true,
        checked: accounts.length,
        failed: accounts.filter((account) => !['available', 'redeemed'].includes(account.row.status)).length,
        mock: true,
      },
      updated_at: now,
    }),
    updated_at: now,
  },
  {
    key: 'redeem_rate_limit',
    value_json: JSON.stringify({
      mock: true,
      enabled: true,
      window_seconds: 60,
      max_requests: 12,
      whitelist_ips: ['127.0.0.1', '::1'],
      updated_at: now,
    }),
    updated_at: now,
  },
]

const accountExports = [
  {
    id: 'mock-export-default-01',
    format: 'cpa',
    source: 'redeem',
    account_ids_json: JSON.stringify(redemptions.default.accountIds),
    account_count: redemptions.default.accountIds.length,
    created_at: redemptions.default.createdAt,
  },
  {
    id: 'mock-export-pro-after-sale-01',
    format: 'cpa',
    source: 'after_sale',
    account_ids_json: JSON.stringify(redemptions.proReplacement.accountIds),
    account_count: redemptions.proReplacement.accountIds.length,
    created_at: redemptions.proReplacement.createdAt,
  },
]

const statements = [
  'PRAGMA foreign_keys = ON;',
  'BEGIN IMMEDIATE;',
  "INSERT OR IGNORE INTO account_pools (id, name, workspace_label, account_type, description, is_default, is_active, created_at, updated_at) VALUES ('default', '默认 Codex 号池', '默认工作区', 'codex', '旧账号和未指定池的默认归属', 1, 1, CAST(strftime('%s', 'now') AS INTEGER), CAST(strftime('%s', 'now') AS INTEGER));",
  "DELETE FROM redeem_after_sales WHERE id LIKE 'mock-%' OR code_id LIKE 'mock-code-%' OR batch_id LIKE 'mock-batch-%';",
  "DELETE FROM redeem_redemptions WHERE id LIKE 'mock-%' OR code_id LIKE 'mock-code-%' OR batch_id LIKE 'mock-batch-%';",
  "DELETE FROM redeem_codes WHERE id LIKE 'mock-code-%' OR batch_id LIKE 'mock-batch-%';",
  "DELETE FROM redeem_code_batches WHERE id LIKE 'mock-batch-%';",
  "DELETE FROM account_health_checks WHERE id LIKE 'mock-%' OR account_id LIKE 'mock-account-%';",
  "DELETE FROM account_exports WHERE id LIKE 'mock-%';",
  "DELETE FROM accounts WHERE id LIKE 'mock-account-%';",
  "DELETE FROM account_pools WHERE id LIKE 'mock-pool-%';",
  "DELETE FROM app_settings WHERE key IN ('auto_probe', 'redeem_rate_limit');",
  ...pools.map((row) => insert('account_pools', row)),
  ...accounts.map((account) => insert('accounts', account.row)),
  ...healthChecks.map((row) => insert('account_health_checks', row)),
  ...batches.map((row) => insert('redeem_code_batches', row)),
  ...codes.map((row) => insert('redeem_codes', row)),
  ...redemptionRows.map((row) => insert('redeem_redemptions', row)),
  ...afterSales.map((row) => insert('redeem_after_sales', row)),
  ...accountExports.map((row) => insert('account_exports', row)),
  ...settings.map((row) => insert('app_settings', row)),
  'COMMIT;',
]

const result = spawnSync('sqlite3', [dbPath], {
  input: `${statements.join('\n')}\n`,
  encoding: 'utf8',
  maxBuffer: 1024 * 1024 * 10,
})

if (result.status !== 0) {
  console.error(result.stderr || result.stdout)
  process.exit(result.status || 1)
}

console.log(`Seeded mock data into ${dbPath}`)
console.log(`pools=${pools.length + 1}, accounts=${accounts.length}, batches=${batches.length}, codes=${codes.length}, redemptions=${redemptionRows.length}, after_sales=${afterSales.length}`)
console.log('sample_codes=MOCK-DEMO-2026-0002, MOCK-PROA-2026-0002, MOCK-FREE-2026-0001')
