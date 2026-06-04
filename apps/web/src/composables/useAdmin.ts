import { computed, reactive, ref } from 'vue'
import {
  api,
  type AccountPoolStats,
  type AccountSummary,
  type AutoProbeSettings,
  type DeleteAccountsResponse,
  type EncodedDownload,
  type ExportFormat,
  type ProxyTestResponse,
  type RedeemBatch,
  type RedeemCode,
  type RedeemRateLimitSettings,
} from '../api/client'

const adminTokenStorageKey = 'aether-pool.admin-token'
localStorage.removeItem(adminTokenStorageKey)

const adminToken = ref(sessionStorage.getItem(adminTokenStorageKey) || '')
const adminTokenDraft = ref('')
const busy = ref(false)
const accounts = ref<AccountSummary[]>([])
const selectedIds = ref<string[]>([])
const importText = ref('')
const adminResult = ref('')
const adminExportFormat = ref<ExportFormat>('cpa')
const batches = ref<RedeemBatch[]>([])
const batchCodes = ref<RedeemCode[]>([])
const selectedBatchId = ref('')
const generatedCodes = ref('')
const autoProbeSettings = ref<AutoProbeSettings | null>(null)
const autoProbeNextRunAt = ref<number | null>(null)
const autoProbeResult = ref('')
const redeemRateLimitSettings = ref<RedeemRateLimitSettings | null>(null)
const redeemRateLimitResult = ref('')
const proxyTestResult = ref<ProxyTestResponse | null>(null)
const proxyTestError = ref('')
const activeView = ref<'accounts' | 'codes'>('accounts')
const filters = reactive({ search: '', status: '', redeemed: '' })
const accountPagination = reactive({
  total: 0,
  limit: 50,
  offset: 0,
})
const accountStats = reactive<AccountPoolStats>({
  total: 0,
  available: 0,
  redeemed: 0,
  attention: 0,
})
const batchForm = reactive({
  name: '',
  total_count: 10,
  accounts_per_code: 1,
  expires_at_text: '',
})
const autoProbeForm = reactive({
  enabled: false,
  interval_seconds: 60 * 60,
  max_accounts_per_run: 100,
  concurrency: 4,
  refresh_before_probe: false,
  proxy_enabled: false,
  proxy_mode: 'fixed' as 'fixed' | 'api',
  proxy_url: '',
  proxy_api_url: '',
  proxy_default_scheme: 'http' as 'http' | 'socks5' | 'socks5h',
})
const redeemRateLimitForm = reactive({
  enabled: true,
  window_seconds: 60,
  max_requests: 30,
  whitelist_text: '',
})
const redeemedAccountDeletableStatuses = new Set(['at_expired', 'refresh_failed', 'auth_invalid', 'forbidden'])

const adminAuthenticated = computed(() => adminToken.value.trim().length > 0)
const apiState = computed(() => ({ token: adminToken.value }))
const availableCount = computed(() => accountStats.available)
const redeemedCount = computed(() => accountStats.redeemed)
const attentionCount = computed(() => accountStats.attention)
const allSelected = computed(() => accounts.value.length > 0 && accounts.value.every((a) => selectedIds.value.includes(a.id)))
const accountPageStart = computed(() => accountPagination.total ? accountPagination.offset + 1 : 0)
const accountPageEnd = computed(() => Math.min(accountPagination.offset + accounts.value.length, accountPagination.total))
const accountCurrentPage = computed(() => accountPagination.total ? Math.floor(accountPagination.offset / accountPagination.limit) + 1 : 1)
const accountTotalPages = computed(() => Math.max(1, Math.ceil(accountPagination.total / accountPagination.limit)))
const canPrevAccountsPage = computed(() => accountPagination.offset > 0)
const canNextAccountsPage = computed(() => accountPagination.offset + accountPagination.limit < accountPagination.total)
const proxyTestDisabled = computed(() => {
  if (busy.value) return true
  if (!autoProbeForm.proxy_enabled) return false
  if (autoProbeForm.proxy_mode === 'api') return !autoProbeForm.proxy_api_url.trim()
  return !autoProbeForm.proxy_url.trim()
})
const batchCodesText = computed(() => batchCodes.value.length ? JSON.stringify(batchCodes.value, null, 2) : '选择批次查看兑换码状态')
const selectedBatch = computed(() => batches.value.find((batch) => batch.id === selectedBatchId.value) || null)
const redeemStats = computed(() => {
  const totalBatches = batches.value.length
  const totalCodes = batches.value.reduce((sum, batch) => sum + Number(batch.total_count || 0), 0)
  const redeemedCodes = batches.value.reduce((sum, batch) => sum + Number(batch.redeemed_count || 0), 0)
  const redeemedAccounts = batches.value.reduce(
    (sum, batch) => sum + Number(batch.redeemed_count || 0) * Number(batch.accounts_per_code || 0),
    0,
  )
  const activeBatches = batches.value.filter((batch) => batch.status === 'active').length
  const expiredBatches = batches.value.filter((batch) => batch.expires_at && batch.expires_at <= Math.floor(Date.now() / 1000)).length
  return {
    totalBatches,
    activeBatches,
    expiredBatches,
    totalCodes,
    redeemedCodes,
    availableCodes: Math.max(totalCodes - redeemedCodes, 0),
    redeemedAccounts,
    redemptionRate: totalCodes ? Math.round((redeemedCodes / totalCodes) * 100) : 0,
  }
})
const selectedBatchStats = computed(() => {
  const totalCodes = batchCodes.value.length
  const redeemedCodes = batchCodes.value.filter((code) => code.status === 'redeemed' || code.redeemed_at).length
  return {
    totalCodes,
    redeemedCodes,
    activeCodes: batchCodes.value.filter((code) => code.status === 'active' && !code.redeemed_at).length,
    disabledCodes: batchCodes.value.filter((code) => code.status === 'disabled').length,
    redemptionRate: totalCodes ? Math.round((redeemedCodes / totalCodes) * 100) : 0,
  }
})
const autoProbeIntervalMinutes = computed({
  get: () => Math.max(1, Math.round(Number(autoProbeForm.interval_seconds || 60) / 60)),
  set: (value: number) => {
    const minutes = Number.isFinite(Number(value)) ? Number(value) : 1
    autoProbeForm.interval_seconds = Math.max(60, Math.round(minutes) * 60)
  },
})

export function useAdmin() {
  return {
    adminToken,
    adminTokenDraft,
    busy,
    accounts,
    selectedIds,
    importText,
    adminResult,
    adminExportFormat,
    batches,
    batchCodes,
    selectedBatchId,
    generatedCodes,
    autoProbeSettings,
    autoProbeNextRunAt,
    autoProbeResult,
    redeemRateLimitSettings,
    redeemRateLimitResult,
    proxyTestResult,
    proxyTestError,
    autoProbeForm,
    autoProbeIntervalMinutes,
    redeemRateLimitForm,
    activeView,
    filters,
    accountPagination,
    accountStats,
    batchForm,
    adminAuthenticated,
    apiState,
    availableCount,
    redeemedCount,
    attentionCount,
    allSelected,
    accountPageStart,
    accountPageEnd,
    accountCurrentPage,
    accountTotalPages,
    canPrevAccountsPage,
    canNextAccountsPage,
    proxyTestDisabled,
    batchCodesText,
    selectedBatch,
    redeemStats,
    selectedBatchStats,
  }
}

export async function withBusy(task: () => Promise<void>, onError?: (msg: string) => void) {
  busy.value = true
  try {
    await task()
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    if (onError) onError(message)
    else adminResult.value = message
  } finally {
    busy.value = false
  }
}

export async function loginAdmin() {
  await withBusy(async () => {
    adminResult.value = ''
    adminToken.value = adminTokenDraft.value.trim()
    sessionStorage.setItem(adminTokenStorageKey, adminToken.value)
    await loadAccounts()
    await loadBatches()
    await loadAutoProbeSettings()
    await loadRedeemRateLimitSettings()
  })
}

export function logoutAdmin() {
  adminToken.value = ''
  adminTokenDraft.value = ''
  accounts.value = []
  batches.value = []
  batchCodes.value = []
  selectedBatchId.value = ''
  generatedCodes.value = ''
  autoProbeSettings.value = null
  autoProbeNextRunAt.value = null
  autoProbeResult.value = ''
  redeemRateLimitSettings.value = null
  redeemRateLimitResult.value = ''
  accountPagination.total = 0
  accountPagination.offset = 0
  applyAccountStats()
  selectedIds.value = []
  sessionStorage.removeItem(adminTokenStorageKey)
}

export async function loadAccounts() {
  const page = await api.listAccounts(apiState.value, {
    ...filters,
    limit: accountPagination.limit,
    offset: accountPagination.offset,
  })
  if (page.total > 0 && page.items.length === 0 && accountPagination.offset >= page.total) {
    accountPagination.offset = Math.max(0, Math.floor((page.total - 1) / accountPagination.limit) * accountPagination.limit)
    return loadAccounts()
  }
  accounts.value = page.items
  accountPagination.total = page.total
  accountPagination.limit = page.limit
  accountPagination.offset = page.offset
  applyAccountStats(page.stats)
  selectedIds.value = selectedIds.value.filter((id) => accounts.value.some((a) => a.id === id))
}

export async function searchAccounts() {
  accountPagination.offset = 0
  await loadAccounts()
}

export async function previousAccountsPage() {
  if (!canPrevAccountsPage.value) return
  accountPagination.offset = Math.max(0, accountPagination.offset - accountPagination.limit)
  await loadAccounts()
}

export async function nextAccountsPage() {
  if (!canNextAccountsPage.value) return
  accountPagination.offset += accountPagination.limit
  await loadAccounts()
}

export async function changeAccountsPageSize() {
  accountPagination.limit = Number(accountPagination.limit || 50)
  accountPagination.offset = 0
  await loadAccounts()
}

export async function loadBatches() {
  const result = await api.listBatches(apiState.value)
  batches.value = result.items
}

export async function loadAutoProbeSettings() {
  const result = await api.getAutoProbeSettings(apiState.value)
  applyAutoProbeSettings(result.settings, result.next_run_at ?? null)
}

export async function loadRedeemRateLimitSettings() {
  const result = await api.getRedeemRateLimitSettings(apiState.value)
  applyRedeemRateLimitSettings(result.settings)
}

export async function refreshAdmin() {
  await withBusy(async () => {
    if (activeView.value === 'codes') {
      await loadBatches()
      if (selectedBatchId.value) await fetchCodes(selectedBatchId.value)
    } else {
      await loadAccounts()
    }
    await loadAutoProbeSettings()
    await loadRedeemRateLimitSettings()
  })
}

export async function importAccounts() {
  await withBusy(async () => {
    const result = await api.importAccounts(apiState.value, importText.value)
    adminResult.value = JSON.stringify(result, null, 2)
    importText.value = ''
    await loadAccounts()
  })
}

export async function probeSelected() {
  await withBusy(async () => {
    const ids = selectedIds.value.length ? selectedIds.value : undefined
    const result = await api.probeAccounts(apiState.value, ids)
    adminResult.value = JSON.stringify(result, null, 2)
    await loadAccounts()
  })
}

export async function probeAccount(accountId: string) {
  await withBusy(async () => {
    const result = await api.probeAccounts(apiState.value, [accountId])
    adminResult.value = JSON.stringify(result, null, 2)
    await loadAccounts()
  })
}

export async function refreshSelected() {
  await withBusy(async () => {
    const ids = selectedIds.value.length ? selectedIds.value : undefined
    const result = await api.refreshAccounts(apiState.value, ids)
    adminResult.value = JSON.stringify(result, null, 2)
    await loadAccounts()
  })
}

export async function deleteSelectedAccounts() {
  const ids = selectedIds.value.slice()
  if (!ids.length) {
    adminResult.value = '请选择要删除的账号'
    return
  }
  if (!window.confirm(`确认删除选中的 ${ids.length} 个账号？未兑换账号会直接删除；已兑换账号仅在 AT 过期、刷新失败、账号失效或网络受限时删除，其余会跳过。`)) {
    return
  }
  await deleteAccountsByIds(ids)
}

export async function deleteAccount(accountId: string) {
  if (!window.confirm('确认删除这个账号？未兑换账号会直接删除；已兑换账号仅在 AT 过期、刷新失败、账号失效或网络受限时删除。')) return
  await deleteAccountsByIds([accountId])
}

async function deleteAccountsByIds(ids: string[]) {
  await withBusy(async () => {
    const result = await api.deleteAccounts(apiState.value, ids)
    adminResult.value = formatDeleteAccountsResult(result)
    selectedIds.value = selectedIds.value.filter(
      (id) => !result.results.some((item) => item.account_id === id && item.status === 'deleted'),
    )
    await loadAccounts()
  })
}

export async function saveAutoProbeSettings() {
  await withBusy(async () => {
    const result = await api.updateAutoProbeSettings(apiState.value, {
      enabled: autoProbeForm.enabled,
      interval_seconds: Number(autoProbeForm.interval_seconds || 60),
      max_accounts_per_run: Number(autoProbeForm.max_accounts_per_run || 1),
      concurrency: Number(autoProbeForm.concurrency || 1),
      refresh_before_probe: false,
      proxy_enabled: autoProbeForm.proxy_enabled,
      proxy_mode: autoProbeForm.proxy_mode,
      proxy_url: autoProbeForm.proxy_url.trim() || null,
      proxy_api_url: autoProbeForm.proxy_api_url.trim() || null,
      proxy_default_scheme: autoProbeForm.proxy_default_scheme,
    })
    applyAutoProbeSettings(result.settings, result.next_run_at ?? null)
    autoProbeResult.value = JSON.stringify({ saved: true, settings: result.settings }, null, 2)
  })
}

export async function saveRedeemRateLimitSettings() {
  await withBusy(async () => {
    const whitelistIps = redeemRateLimitForm.whitelist_text
      .split(/\r?\n|,/)
      .map((value) => value.trim())
      .filter(Boolean)
    const result = await api.updateRedeemRateLimitSettings(apiState.value, {
      enabled: redeemRateLimitForm.enabled,
      window_seconds: Number(redeemRateLimitForm.window_seconds || 1),
      max_requests: Number(redeemRateLimitForm.max_requests || 1),
      whitelist_ips: whitelistIps,
    })
    applyRedeemRateLimitSettings(result.settings)
    redeemRateLimitResult.value = result.settings.enabled
      ? `已保存：每 ${result.settings.window_seconds} 秒最多 ${result.settings.max_requests} 次请求，白名单 ${result.settings.whitelist_ips.length} 个。`
      : '已保存：兑换限速已关闭。'
  })
}

export async function testProxyEgress() {
  await withBusy(
    async () => {
      proxyTestResult.value = null
      proxyTestError.value = ''
      proxyTestResult.value = await api.testProxyEgress(apiState.value, {
        enabled: autoProbeForm.enabled,
        interval_seconds: Number(autoProbeForm.interval_seconds || 60),
        max_accounts_per_run: Number(autoProbeForm.max_accounts_per_run || 1),
        concurrency: Number(autoProbeForm.concurrency || 1),
        refresh_before_probe: false,
        proxy_enabled: autoProbeForm.proxy_enabled,
        proxy_mode: autoProbeForm.proxy_mode,
        proxy_url: autoProbeForm.proxy_url.trim() || null,
        proxy_api_url: autoProbeForm.proxy_api_url.trim() || null,
        proxy_default_scheme: autoProbeForm.proxy_default_scheme,
      })
    },
    (message) => {
      proxyTestError.value = message
    },
  )
}

export async function runAutoProbeNow() {
  await withBusy(async () => {
    const result = await api.runAutoProbeNow(apiState.value)
    applyAutoProbeSettings(result.settings.settings, result.settings.next_run_at ?? null)
    autoProbeResult.value = JSON.stringify(result.run, null, 2)
    await loadAccounts()
  })
}

export async function exportSelected() {
  await withBusy(async () => {
    const result = await api.exportAccounts(apiState.value, {
      account_ids: selectedIds.value.length ? selectedIds.value : undefined,
      include_redeemed: false,
      format: adminExportFormat.value,
    })
    adminResult.value = JSON.stringify(exportResultForDisplay(result), null, 2)
    if (result.download) downloadEncodedFile(result.download)
    else downloadJson(`account-pool-admin-${result.format}-${timestamp()}.json`, result.document)
  })
}

export function toggleAll(event: Event) {
  const checked = (event.target as HTMLInputElement).checked
  selectedIds.value = checked ? accounts.value.map((a) => a.id) : []
}

export async function createBatch() {
  await withBusy(async () => {
    const expiresAt = batchForm.expires_at_text.trim()
      ? Math.floor(new Date(batchForm.expires_at_text.trim()).getTime() / 1000)
      : null
    const result = await api.createBatch(apiState.value, {
      name: batchForm.name || `兑换码批次 ${new Date().toLocaleString()}`,
      total_count: Number(batchForm.total_count || 1),
      accounts_per_code: Number(batchForm.accounts_per_code || 1),
      expires_at: Number.isFinite(expiresAt) ? expiresAt : null,
      plan_filter: null,
    })
    generatedCodes.value = result.codes.map((c) => c.code).join('\n')
    await loadBatches()
    await fetchCodes(result.batch_id)
  })
}

export async function loadCodes(batchId: string) {
  await withBusy(() => fetchCodes(batchId))
}

async function fetchCodes(batchId: string) {
  selectedBatchId.value = batchId
  const result = await api.listCodes(apiState.value, batchId)
  batchCodes.value = result.items
}

function applyAutoProbeSettings(settings: AutoProbeSettings, nextRunAt: number | null) {
  autoProbeSettings.value = settings
  autoProbeNextRunAt.value = nextRunAt
  autoProbeForm.enabled = settings.enabled
  autoProbeForm.interval_seconds = settings.interval_seconds
  autoProbeForm.max_accounts_per_run = settings.max_accounts_per_run
  autoProbeForm.concurrency = settings.concurrency
  autoProbeForm.refresh_before_probe = false
  autoProbeForm.proxy_enabled = Boolean(settings.proxy_enabled)
  autoProbeForm.proxy_mode = settings.proxy_mode || 'fixed'
  autoProbeForm.proxy_url = settings.proxy_url || ''
  autoProbeForm.proxy_api_url = settings.proxy_api_url || ''
  autoProbeForm.proxy_default_scheme = settings.proxy_default_scheme || 'http'
}

function applyRedeemRateLimitSettings(settings: RedeemRateLimitSettings) {
  redeemRateLimitSettings.value = settings
  redeemRateLimitForm.enabled = settings.enabled
  redeemRateLimitForm.window_seconds = settings.window_seconds
  redeemRateLimitForm.max_requests = settings.max_requests
  redeemRateLimitForm.whitelist_text = (settings.whitelist_ips || []).join('\n')
}

function applyAccountStats(stats?: AccountPoolStats) {
  accountStats.total = Number(stats?.total || 0)
  accountStats.available = Number(stats?.available || 0)
  accountStats.redeemed = Number(stats?.redeemed || 0)
  accountStats.attention = Number(stats?.attention || 0)
}

export function exportResultForDisplay<T extends { download?: EncodedDownload | null }>(result: T) {
  if (!result.download) return result
  return {
    ...result,
    download: {
      filename: normalizeDownloadFileName(result.download.filename),
      content_type: result.download.content_type,
      encoding: result.download.encoding,
      data: `<base64 ${result.download.data.length} chars>`,
    },
  }
}

function formatDeleteAccountsResult(result: DeleteAccountsResponse) {
  const lines = [
    `删除完成：已删除 ${result.deleted} 个，跳过 ${result.skipped} 个，不存在 ${result.not_found} 个。`,
  ]
  const skipped = result.results.filter((item) => item.status !== 'deleted')
  if (skipped.length) {
    lines.push('明细：')
    lines.push(
      ...skipped.map((item) => {
        const reason = item.reason ? `，${item.reason}` : ''
        return `- ${item.account_id}：${item.status}${reason}`
      }),
    )
  }
  return lines.join('\n')
}

export function downloadEncodedFile(download: EncodedDownload) {
  if (download.encoding !== 'base64') return
  const binary = atob(download.data)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
  const blob = new Blob([bytes], { type: download.content_type || 'application/octet-stream' })
  downloadBlob(normalizeDownloadFileName(download.filename), blob)
}

export function downloadJson(fileName: string, value: unknown) {
  const blob = new Blob([`${JSON.stringify(value, null, 2)}\n`], { type: 'application/json;charset=utf-8' })
  downloadBlob(fileName, blob)
}

export function downloadText(fileName: string, value: string, contentType = 'text/plain;charset=utf-8') {
  const blob = new Blob([value.endsWith('\n') ? value : `${value}\n`], { type: contentType })
  downloadBlob(fileName, blob)
}

function downloadBlob(fileName: string, blob: Blob) {
  const url = URL.createObjectURL(blob)
  const link = document.createElement('a')
  link.href = url
  link.download = fileName
  document.body.append(link)
  link.click()
  link.remove()
  URL.revokeObjectURL(url)
}

export function timestamp() {
  return new Date().toISOString().replace(/[:.]/g, '-')
}

export function normalizeDownloadFileName(fileName: string) {
  return fileName.replace(/^aether-pool/i, 'account-pool')
}

export function statusLabel(status: string) {
  return ({
    active: '可兑换',
    available: '可用',
    at_expired: 'AT 过期',
    refresh_failed: '刷新失败',
    auth_invalid: '账号失效',
    forbidden: '网络受限',
    quota_exhausted: '额度耗尽',
    redeemed: '待测活',
    disabled: '已停用',
  } as Record<string, string>)[status] || status
}

export function statusBadgeClass(status: string) {
  return status === 'redeemed' ? 'disabled' : status
}

type QuotaWindow = 'five_hour' | 'weekly'

function quotaPercentValue(account: AccountSummary, window: QuotaWindow) {
  const key = window === 'five_hour' ? 'primary_used_percent' : 'secondary_used_percent'
  const raw = account.quota_snapshot?.[key]
  const value = typeof raw === 'number' ? raw : typeof raw === 'string' ? Number.parseFloat(raw) : NaN
  return Number.isFinite(value) ? Math.max(0, value) : null
}

export function quotaPercentText(account: AccountSummary, window: QuotaWindow) {
  const value = quotaPercentValue(account, window)
  if (value === null) return '-'
  const rounded = Math.round(value * 10) / 10
  return `${Number.isInteger(rounded) ? rounded.toFixed(0) : rounded.toFixed(1)}%`
}

export function quotaPercentBarWidth(account: AccountSummary, window: QuotaWindow) {
  const value = quotaPercentValue(account, window)
  return `${Math.min(value ?? 0, 100)}%`
}

export function quotaUsageClass(account: AccountSummary, window: QuotaWindow) {
  const value = quotaPercentValue(account, window)
  if (value === null) return 'empty'
  if (value >= 100) return 'exhausted'
  if (value >= 80) return 'warning'
  return 'ok'
}

function accountRedeemed(account: AccountSummary) {
  return Boolean(account.redeemed_at || account.redeem_code_id || account.redemption_id)
}

export function canDeleteAccount(account: AccountSummary) {
  return !accountRedeemed(account) || redeemedAccountDeletableStatuses.has(account.status)
}

export function formatTime(value?: number | null) {
  if (!value) return '-'
  return new Date(value * 1000).toLocaleString()
}
