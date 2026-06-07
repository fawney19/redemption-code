export type ExportFormat = 'cpa' | 'sub2api'

export interface AccountPool {
  id: string
  name: string
  workspace_label?: string | null
  account_type?: string | null
  description?: string | null
  is_default: boolean
  is_active: boolean
  created_at: number
  updated_at: number
}

export interface AccountPoolPayload {
  name: string
  workspace_label?: string | null
  account_type?: string | null
  description?: string | null
  is_active?: boolean | null
}

export interface AccountSummary {
  id: string
  pool_id: string
  pool_name?: string | null
  email?: string | null
  name?: string | null
  account_id?: string | null
  plan_type?: string | null
  status: string
  access_token_preview?: string | null
  refresh_token_preview?: string | null
  expires_at?: number | null
  last_refresh_at?: number | null
  last_probe_at?: number | null
  quota_snapshot?: Record<string, unknown> | null
  redeem_code_id?: string | null
  redeem_code_masked?: string | null
  redemption_id?: string | null
  redeemed_at?: number | null
  created_at: number
  updated_at: number
}

export interface AccountListPage {
  items: AccountSummary[]
  total: number
  limit: number
  offset: number
  stats: AccountPoolStats
}

export interface AccountPoolStats {
  total: number
  available: number
  redeemed: number
  attention: number
}

export interface RedeemBatch {
  id: string
  pool_id: string
  pool_name?: string | null
  name: string
  status: string
  total_count: number
  redeemed_count: number
  accounts_per_code: number
  after_sale_limit: number
  plan_filter: string[]
  expires_at?: number | null
  created_at: number
}

export interface RedeemCodeAccount {
  id: string
  pool_id: string
  pool_name?: string | null
  email?: string | null
  name?: string | null
  account_id?: string | null
  plan_type?: string | null
  status: string
  last_probe_at?: number | null
  quota_snapshot?: Record<string, unknown> | null
}

export interface RedeemAfterSale {
  id: string
  status: string
  reason?: string | null
  old_accounts: RedeemCodeAccount[]
  new_accounts: RedeemCodeAccount[]
  created_at: number
}

export interface RedeemCode {
  id: string
  batch_id: string
  code?: string | null
  masked_code: string
  status: string
  redemption_id?: string | null
  redeemed_at?: number | null
  after_sale_count: number
  after_sales: RedeemAfterSale[]
  accounts: RedeemCodeAccount[]
  created_at: number
  updated_at: number
}

export interface ApiState {
  token: string
}

export interface EncodedDownload {
  filename: string
  content_type: string
  encoding: 'base64'
  data: string
}

export interface ExportResponse {
  document: unknown
  format: ExportFormat
  download?: EncodedDownload | null
}

export interface RedeemSuccess {
  code: string
  account_count: number
  after_sale_count?: number | null
  replacement_account_count?: number | null
}

export interface RedeemFailure {
  code: string
  reason: string
}

export interface RedeemExportResponse extends ExportResponse {
  successes: RedeemSuccess[]
  failures: RedeemFailure[]
}

export interface RedeemJob {
  id: string
  mode: 'redeem' | 'after_sale' | string
  format: ExportFormat
  status: 'queued' | 'running' | 'completed' | 'failed' | string
  total_codes: number
  processed_codes: number
  progress: number
  success_count: number
  failure_count: number
  account_count: number
  network_total: number
  network_done: number
  message?: string | null
  error?: string | null
  result?: RedeemExportResponse | null
}

export interface RedeemJobResponse {
  success: boolean
  job: RedeemJob
}

export type ProbeMode = 'hybrid' | 'direct' | 'cpa'

export interface AutoProbeSettings {
  enabled: boolean
  interval_seconds: number
  max_accounts_per_run: number
  concurrency: number
  refresh_before_probe: boolean
  probe_mode: ProbeMode
  deep_check_enabled: boolean
  cpa_base_url?: string | null
  cpa_management_key_set: boolean
  proxy_enabled: boolean
  proxy_mode: 'fixed' | 'api'
  proxy_url?: string | null
  proxy_api_url?: string | null
  proxy_default_scheme: 'http' | 'socks5' | 'socks5h'
  last_started_at?: number | null
  last_finished_at?: number | null
  last_checked_count: number
  last_error?: string | null
  last_result?: unknown | null
  updated_at: number
}

export interface AutoProbeSettingsResponse {
  success: boolean
  settings: AutoProbeSettings
  next_run_at?: number | null
}

export interface AutoProbeRunResponse {
  success: boolean
  settings: AutoProbeSettingsResponse
  run: {
    success: boolean
    checked: number
    failed: number
    results: unknown[]
    truncated: boolean
  }
}

export interface ProxyTestResponse {
  success: boolean
  ip: string
  proxy?: string | null
  mode: 'direct' | 'fixed' | 'api' | string
  url: string
  elapsed_ms: number
}

export interface CpaTestResponse {
  success: boolean
  base_url: string
  auth_file_count: number
  elapsed_ms: number
}

export interface CpaScanResponse {
  success: boolean
  total: number
  max_items: number
  results: unknown[]
  diagnostics: unknown[]
  summary: {
    total: number
    ready: number
    error_accounts: number
    failed: number
  }
}

export interface DeleteAccountsResponse {
  success: boolean
  deleted: number
  skipped: number
  not_found: number
  results: Array<{
    account_id: string
    status: 'deleted' | 'skipped' | 'not_found' | string
    reason?: string | null
  }>
}

export interface DeleteRedeemBatchResponse {
  success: boolean
  message: string
  deleted: boolean
  accounts_reset: number
  codes_deleted: number
  redemptions_deleted: number
  after_sales_deleted: number
}

export interface AccountTokenResponse {
  success: boolean
  account_id: string
  kind: 'access_token' | 'refresh_token'
  token: string
}

export interface RedeemRateLimitSettings {
  enabled: boolean
  window_seconds: number
  max_requests: number
  whitelist_ips: string[]
  updated_at: number
}

export interface RedeemRateLimitSettingsResponse {
  success: boolean
  settings: RedeemRateLimitSettings
}

const API_BASE = (import.meta.env.VITE_API_BASE || '').replace(/\/+$/, '')
const MANAGEMENT_API_PREFIX = '/api/alalalateam'

function apiUrl(path: string): string {
  if (/^https?:\/\//i.test(path)) return path
  return `${API_BASE}${path}`
}

async function request<T>(
  path: string,
  state: ApiState,
  options: RequestInit = {},
): Promise<T> {
  const headers = new Headers(options.headers)
  headers.set('Content-Type', 'application/json')
  if (state.token.trim()) headers.set('Authorization', `Bearer ${state.token.trim()}`)
  const response = await fetch(apiUrl(path), { ...options, headers })
  const text = await response.text()
  const data = text ? JSON.parse(text) : {}
  if (!response.ok) {
    throw new Error(data.error || `${path} failed (${response.status})`)
  }
  return data as T
}

export const api = {
  listAccounts(state: ApiState, params: {
    pool_id?: string
    pool_ids?: string[]
    search?: string
    status?: string
    statuses?: string[]
    redeemed?: string
    redeemed_values?: string[]
    limit?: number
    offset?: number
  }) {
    const query = new URLSearchParams({
      limit: String(params.limit || 50),
      offset: String(params.offset || 0),
    })
    if (params.search) query.set('search', params.search)
    if (params.status) query.set('status', params.status)
    if (params.statuses?.length) query.set('statuses', params.statuses.join(','))
    if (params.redeemed) query.set('redeemed', params.redeemed)
    if (params.redeemed_values?.length) query.set('redeemed_values', params.redeemed_values.join(','))
    if (params.pool_id) query.set('pool_id', params.pool_id)
    if (params.pool_ids?.length) query.set('pool_ids', params.pool_ids.join(','))
    return request<AccountListPage>(`${MANAGEMENT_API_PREFIX}/accounts?${query}`, state)
  },
  listPools(state: ApiState, params: { active_only?: boolean } = {}) {
    const query = new URLSearchParams()
    if (params.active_only) query.set('active_only', 'true')
    const suffix = query.toString() ? `?${query}` : ''
    return request<{ success: boolean; items: AccountPool[]; default_pool_id: string }>(
      `${MANAGEMENT_API_PREFIX}/pools${suffix}`,
      state,
    )
  },
  createPool(state: ApiState, payload: AccountPoolPayload) {
    return request<{ success: boolean; pool: AccountPool }>(`${MANAGEMENT_API_PREFIX}/pools`, state, {
      method: 'POST',
      body: JSON.stringify(payload),
    })
  },
  updatePool(state: ApiState, poolId: string, payload: AccountPoolPayload) {
    return request<{ success: boolean; pool: AccountPool }>(`${MANAGEMENT_API_PREFIX}/pools/${poolId}`, state, {
      method: 'POST',
      body: JSON.stringify(payload),
    })
  },
  importAccounts(state: ApiState, credentials: string, poolId?: string) {
    return request<{ imported: number; updated: number; parse_errors: string[] }>(
      `${MANAGEMENT_API_PREFIX}/accounts/import`,
      state,
      { method: 'POST', body: JSON.stringify({ credentials, pool_id: poolId || undefined }) },
    )
  },
  probeAccounts(state: ApiState, accountIds?: string[], poolId?: string) {
    return request<{ results: unknown[] }>(`${MANAGEMENT_API_PREFIX}/accounts/probe`, state, {
      method: 'POST',
      body: JSON.stringify({ account_ids: accountIds, pool_id: poolId || undefined }),
    })
  },
  refreshAccounts(state: ApiState, accountIds?: string[], poolId?: string) {
    return request<{ refreshed: number; skipped: number; failed: number; results: unknown[] }>(
      `${MANAGEMENT_API_PREFIX}/accounts/refresh`,
      state,
      { method: 'POST', body: JSON.stringify({ account_ids: accountIds, pool_id: poolId || undefined }) },
    )
  },
  deleteAccounts(state: ApiState, accountIds: string[]) {
    return request<DeleteAccountsResponse>(`${MANAGEMENT_API_PREFIX}/accounts/delete`, state, {
      method: 'POST',
      body: JSON.stringify({ account_ids: accountIds }),
    })
  },
  exportAccounts(state: ApiState, payload: { account_ids?: string[]; include_redeemed?: boolean; pool_id?: string; format: ExportFormat }) {
    return request<ExportResponse>(`${MANAGEMENT_API_PREFIX}/accounts/export`, state, {
      method: 'POST',
      body: JSON.stringify(payload),
    })
  },
  getAccountToken(state: ApiState, accountId: string, kind: 'access' | 'refresh') {
    const query = new URLSearchParams({ kind })
    return request<AccountTokenResponse>(
      `${MANAGEMENT_API_PREFIX}/accounts/${encodeURIComponent(accountId)}/token?${query}`,
      state,
    )
  },
  listBatches(state: ApiState, poolId?: string) {
    const query = new URLSearchParams()
    if (poolId) query.set('pool_id', poolId)
    const suffix = query.toString() ? `?${query}` : ''
    return request<{ items: RedeemBatch[] }>(`${MANAGEMENT_API_PREFIX}/redeem-code-batches${suffix}`, state)
  },
  createBatch(state: ApiState, payload: {
    pool_id?: string
    name: string
    total_count: number
    accounts_per_code: number
    after_sale_limit?: number | null
    expires_at?: number | null
    plan_filter?: string[] | null
  }) {
    return request<{ batch_id: string; codes: Array<{ id: string; code: string; masked_code: string }> }>(
      `${MANAGEMENT_API_PREFIX}/redeem-code-batches`,
      state,
      { method: 'POST', body: JSON.stringify(payload) },
    )
  },
  listCodes(state: ApiState, batchId: string) {
    return request<{ items: RedeemCode[] }>(`${MANAGEMENT_API_PREFIX}/redeem-code-batches/${batchId}/codes`, state)
  },
  deleteBatch(state: ApiState, batchId: string) {
    return request<DeleteRedeemBatchResponse>(
      `${MANAGEMENT_API_PREFIX}/redeem-code-batches/${encodeURIComponent(batchId)}`,
      state,
      { method: 'DELETE' },
    )
  },
  getAutoProbeSettings(state: ApiState) {
    return request<AutoProbeSettingsResponse>(`${MANAGEMENT_API_PREFIX}/settings/auto-probe`, state)
  },
  getRedeemRateLimitSettings(state: ApiState) {
    return request<RedeemRateLimitSettingsResponse>(`${MANAGEMENT_API_PREFIX}/settings/redeem-rate-limit`, state)
  },
  updateRedeemRateLimitSettings(state: ApiState, payload: Partial<Pick<
    RedeemRateLimitSettings,
    | 'enabled'
    | 'window_seconds'
    | 'max_requests'
    | 'whitelist_ips'
  >>) {
    return request<RedeemRateLimitSettingsResponse>(`${MANAGEMENT_API_PREFIX}/settings/redeem-rate-limit`, state, {
      method: 'POST',
      body: JSON.stringify(payload),
    })
  },
  updateAutoProbeSettings(state: ApiState, payload: Partial<Pick<
    AutoProbeSettings,
    | 'enabled'
    | 'interval_seconds'
    | 'max_accounts_per_run'
    | 'concurrency'
    | 'refresh_before_probe'
    | 'probe_mode'
    | 'deep_check_enabled'
    | 'cpa_base_url'
    | 'proxy_enabled'
    | 'proxy_mode'
    | 'proxy_url'
    | 'proxy_api_url'
    | 'proxy_default_scheme'
  >> & { cpa_management_key?: string | null }) {
    return request<AutoProbeSettingsResponse>(`${MANAGEMENT_API_PREFIX}/settings/auto-probe`, state, {
      method: 'POST',
      body: JSON.stringify(payload),
    })
  },
  testProxyEgress(state: ApiState, payload: Partial<Pick<
    AutoProbeSettings,
    | 'enabled'
    | 'interval_seconds'
    | 'max_accounts_per_run'
    | 'concurrency'
    | 'refresh_before_probe'
    | 'probe_mode'
    | 'deep_check_enabled'
    | 'cpa_base_url'
    | 'proxy_enabled'
    | 'proxy_mode'
    | 'proxy_url'
    | 'proxy_api_url'
    | 'proxy_default_scheme'
  >>) {
    return request<ProxyTestResponse>(`${MANAGEMENT_API_PREFIX}/settings/proxy/test`, state, {
      method: 'POST',
      body: JSON.stringify(payload),
    })
  },
  testCpaConnection(state: ApiState, payload: Partial<Pick<
    AutoProbeSettings,
    | 'probe_mode'
    | 'deep_check_enabled'
    | 'cpa_base_url'
  >> & { cpa_management_key?: string | null }) {
    return request<CpaTestResponse>(`${MANAGEMENT_API_PREFIX}/settings/cpa/test`, state, {
      method: 'POST',
      body: JSON.stringify(payload),
    })
  },
  scanCpa401(state: ApiState, maxItems = 20) {
    return request<CpaScanResponse>(`${MANAGEMENT_API_PREFIX}/cpa/scan-401`, state, {
      method: 'POST',
      body: JSON.stringify({ max_items: maxItems }),
    })
  },
  runAutoProbeNow(state: ApiState) {
    return request<AutoProbeRunResponse>(`${MANAGEMENT_API_PREFIX}/settings/auto-probe/run`, state, {
      method: 'POST',
      body: JSON.stringify({}),
    })
  },
  redeemExport(payload: { codes: string[]; format: ExportFormat }) {
    return request<RedeemExportResponse>(
      '/api/redeem/export',
      { token: '' },
      { method: 'POST', body: JSON.stringify(payload) },
    )
  },
  redeemAfterSale(payload: { codes: string[]; format: ExportFormat }) {
    return request<RedeemExportResponse>(
      '/api/redeem/after-sale',
      { token: '' },
      { method: 'POST', body: JSON.stringify(payload) },
    )
  },
  startRedeemExportJob(payload: { codes: string[]; format: ExportFormat }) {
    return request<RedeemJobResponse>(
      '/api/redeem/export-jobs',
      { token: '' },
      { method: 'POST', body: JSON.stringify(payload) },
    )
  },
  startRedeemAfterSaleJob(payload: { codes: string[]; format: ExportFormat }) {
    return request<RedeemJobResponse>(
      '/api/redeem/after-sale-jobs',
      { token: '' },
      { method: 'POST', body: JSON.stringify(payload) },
    )
  },
  getRedeemJob(jobId: string) {
    return request<RedeemJobResponse>(`/api/redeem/jobs/${encodeURIComponent(jobId)}`, { token: '' })
  },
}
