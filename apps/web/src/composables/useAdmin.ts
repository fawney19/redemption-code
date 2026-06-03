import { computed, reactive, ref } from 'vue'
import { api, type AccountSummary, type EncodedDownload, type ExportFormat, type RedeemBatch, type RedeemCode } from '../api/client'

const adminToken = ref(localStorage.getItem('aether-pool.admin-token') || '')
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
const activeView = ref<'accounts' | 'codes'>('accounts')
const filters = reactive({ search: '', status: '', redeemed: '' })
const batchForm = reactive({
  name: '',
  total_count: 10,
  accounts_per_code: 1,
  plan_filter_text: '',
  expires_at_text: '',
})

const adminAuthenticated = computed(() => adminToken.value.trim().length > 0)
const apiState = computed(() => ({ token: adminToken.value }))
const availableCount = computed(() => accounts.value.filter((a) => a.status === 'available' && !a.redeemed_at).length)
const redeemedCount = computed(() => accounts.value.filter((a) => a.redeemed_at || a.status === 'redeemed').length)
const attentionCount = computed(() => accounts.value.filter((a) => ['at_expired', 'refresh_failed', 'auth_invalid', 'quota_exhausted'].includes(a.status)).length)
const allSelected = computed(() => accounts.value.length > 0 && accounts.value.every((a) => selectedIds.value.includes(a.id)))
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
    activeView,
    filters,
    batchForm,
    adminAuthenticated,
    apiState,
    availableCount,
    redeemedCount,
    attentionCount,
    allSelected,
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
    localStorage.setItem('aether-pool.admin-token', adminToken.value)
    await loadAccounts()
    await loadBatches()
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
  selectedIds.value = []
  localStorage.removeItem('aether-pool.admin-token')
}

export async function loadAccounts() {
  const page = await api.listAccounts(apiState.value, filters)
  accounts.value = page.items
  selectedIds.value = selectedIds.value.filter((id) => accounts.value.some((a) => a.id === id))
}

export async function loadBatches() {
  const result = await api.listBatches(apiState.value)
  batches.value = result.items
}

export async function refreshAdmin() {
  await withBusy(async () => {
    if (activeView.value === 'codes') {
      await loadBatches()
      if (selectedBatchId.value) await fetchCodes(selectedBatchId.value)
    } else {
      await loadAccounts()
    }
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

export async function refreshSelected() {
  await withBusy(async () => {
    const ids = selectedIds.value.length ? selectedIds.value : undefined
    const result = await api.refreshAccounts(apiState.value, ids)
    adminResult.value = JSON.stringify(result, null, 2)
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
    else downloadJson(`aether-pool-admin-${result.format}-${timestamp()}.json`, result.document)
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
      plan_filter: batchForm.plan_filter_text.split(',').map((s) => s.trim()).filter(Boolean),
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

export function exportResultForDisplay<T extends { download?: EncodedDownload | null }>(result: T) {
  if (!result.download) return result
  return {
    ...result,
    download: {
      filename: result.download.filename,
      content_type: result.download.content_type,
      encoding: result.download.encoding,
      data: `<base64 ${result.download.data.length} chars>`,
    },
  }
}

export function downloadEncodedFile(download: EncodedDownload) {
  if (download.encoding !== 'base64') return
  const binary = atob(download.data)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
  const blob = new Blob([bytes], { type: download.content_type || 'application/octet-stream' })
  downloadBlob(download.filename, blob)
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

export function statusLabel(status: string) {
  return ({
    active: '可兑换',
    available: '可用',
    at_expired: 'AT 过期',
    refresh_failed: '刷新失败',
    auth_invalid: '账号失效',
    quota_exhausted: '额度耗尽',
    redeemed: '已兑换',
    disabled: '已停用',
  } as Record<string, string>)[status] || status
}

export function formatTime(value?: number | null) {
  if (!value) return '-'
  return new Date(value * 1000).toLocaleString()
}
