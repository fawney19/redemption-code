export type ExportFormat = 'cpa' | 'sub2api'

export interface AccountSummary {
  id: string
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
  name: string
  status: string
  total_count: number
  redeemed_count: number
  accounts_per_code: number
  plan_filter: string[]
  expires_at?: number | null
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
  accounts: Array<{
    id: string
    email?: string | null
    name?: string | null
    account_id?: string | null
    plan_type?: string | null
    status: string
    last_probe_at?: number | null
    quota_snapshot?: Record<string, unknown> | null
  }>
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
}

export interface RedeemFailure {
  code: string
  reason: string
}

export interface RedeemExportResponse extends ExportResponse {
  successes: RedeemSuccess[]
  failures: RedeemFailure[]
}

export interface AutoProbeSettings {
  enabled: boolean
  interval_seconds: number
  max_accounts_per_run: number
  concurrency: number
  refresh_before_probe: boolean
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

const API_BASE = import.meta.env.VITE_API_BASE || ''
const MANAGEMENT_API_PREFIX = '/api/alalalateam'

async function request<T>(
  path: string,
  state: ApiState,
  options: RequestInit = {},
): Promise<T> {
  const headers = new Headers(options.headers)
  headers.set('Content-Type', 'application/json')
  if (state.token.trim()) headers.set('Authorization', `Bearer ${state.token.trim()}`)
  const response = await fetch(`${API_BASE}${path}`, { ...options, headers })
  const text = await response.text()
  const data = text ? JSON.parse(text) : {}
  if (!response.ok) {
    throw new Error(data.error || `${path} failed (${response.status})`)
  }
  return data as T
}

export const api = {
  listAccounts(state: ApiState, params: {
    search?: string
    status?: string
    redeemed?: string
    limit?: number
    offset?: number
  }) {
    const query = new URLSearchParams({
      limit: String(params.limit || 50),
      offset: String(params.offset || 0),
    })
    if (params.search) query.set('search', params.search)
    if (params.status) query.set('status', params.status)
    if (params.redeemed) query.set('redeemed', params.redeemed)
    return request<AccountListPage>(`${MANAGEMENT_API_PREFIX}/accounts?${query}`, state)
  },
  importAccounts(state: ApiState, credentials: string) {
    return request<{ imported: number; updated: number; parse_errors: string[] }>(
      `${MANAGEMENT_API_PREFIX}/accounts/import`,
      state,
      { method: 'POST', body: JSON.stringify({ credentials }) },
    )
  },
  probeAccounts(state: ApiState, accountIds?: string[]) {
    return request<{ results: unknown[] }>(`${MANAGEMENT_API_PREFIX}/accounts/probe`, state, {
      method: 'POST',
      body: JSON.stringify({ account_ids: accountIds }),
    })
  },
  refreshAccounts(state: ApiState, accountIds?: string[]) {
    return request<{ refreshed: number; skipped: number; failed: number; results: unknown[] }>(
      `${MANAGEMENT_API_PREFIX}/accounts/refresh`,
      state,
      { method: 'POST', body: JSON.stringify({ account_ids: accountIds }) },
    )
  },
  deleteAccounts(state: ApiState, accountIds: string[]) {
    return request<DeleteAccountsResponse>(`${MANAGEMENT_API_PREFIX}/accounts/delete`, state, {
      method: 'POST',
      body: JSON.stringify({ account_ids: accountIds }),
    })
  },
  exportAccounts(state: ApiState, payload: { account_ids?: string[]; include_redeemed?: boolean; format: ExportFormat }) {
    return request<ExportResponse>(`${MANAGEMENT_API_PREFIX}/accounts/export`, state, {
      method: 'POST',
      body: JSON.stringify(payload),
    })
  },
  listBatches(state: ApiState) {
    return request<{ items: RedeemBatch[] }>(`${MANAGEMENT_API_PREFIX}/redeem-code-batches`, state)
  },
  createBatch(state: ApiState, payload: {
    name: string
    total_count: number
    accounts_per_code: number
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
    | 'proxy_enabled'
    | 'proxy_mode'
    | 'proxy_url'
    | 'proxy_api_url'
    | 'proxy_default_scheme'
  >>) {
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
}
