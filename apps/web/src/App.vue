<template>
  <div v-if="!isAdminRoute" class="public-shell">
    <header class="public-header">
      <div class="brand">
        <div class="brand-mark">A</div>
        <div>
          <strong>AetherPool</strong>
          <span>Codex 账号兑换导出</span>
        </div>
      </div>
    </header>

    <main class="public-main">
      <section class="redeem-workbench">
        <div class="public-title">
          <h1>账号兑换导出</h1>
          <p>输入兑换码，选择格式后下载账号 JSON。</p>
        </div>
        <div class="grid two">
          <div class="panel">
            <div class="panel-header"><h2>兑换码</h2></div>
            <div class="panel-body grid">
              <textarea v-model="redeemText" class="textarea redeem-textarea" placeholder="每行一个兑换码"></textarea>
              <select v-model="redeemFormat" class="select">
                <option value="cpa">CPA JSON</option>
                <option value="sub2api">Sub2API JSON</option>
              </select>
              <button class="button primary" :disabled="busy || !redeemText.trim()" @click="redeemExport">
                <Download />兑换并导出
              </button>
            </div>
          </div>
          <div class="panel">
            <div class="panel-header">
              <h2>结果</h2>
              <button class="button" :disabled="!redeemDocument" @click="downloadRedeem"><FileJson />下载</button>
            </div>
            <div class="panel-body">
              <pre class="result mono">{{ redeemResult || '兑换结果会显示在这里。' }}</pre>
            </div>
          </div>
        </div>
      </section>
    </main>
  </div>

  <div v-else-if="!adminAuthenticated" class="admin-login-shell">
    <div class="login-panel panel">
      <div class="panel-header">
        <div class="brand compact">
          <div class="brand-mark">A</div>
          <div>
            <strong>AetherPool</strong>
            <span>管理后台</span>
          </div>
        </div>
      </div>
      <div class="panel-body grid">
        <input
          v-model="adminTokenDraft"
          class="input"
          type="password"
          autocomplete="current-password"
          placeholder="AETHER_POOL_ADMIN_TOKEN"
          @keyup.enter="loginAdmin"
        />
        <button class="button primary" :disabled="busy || !adminTokenDraft.trim()" @click="loginAdmin">
          进入管理后台
        </button>
        <p class="muted">后台地址不在普通页面展示，所有管理接口都需要管理员令牌。</p>
        <pre v-if="adminResult" class="result mono">{{ adminResult }}</pre>
      </div>
    </div>
  </div>

  <div v-else class="app-shell">
    <aside class="sidebar">
      <div class="brand">
        <div class="brand-mark">A</div>
        <div>
          <strong>AetherPool</strong>
          <span>Codex 账号池管理</span>
        </div>
      </div>
      <button class="nav-button" :class="{ active: activeView === 'accounts' }" @click="activeView = 'accounts'">
        <Database />账号池
      </button>
      <button class="nav-button" :class="{ active: activeView === 'codes' }" @click="activeView = 'codes'">
        <Ticket />兑换码
      </button>
      <button class="nav-button danger" @click="logoutAdmin">
        <RotateCcw />退出后台
      </button>
    </aside>

    <main class="main">
      <div class="topbar">
        <div>
          <h1>{{ pageTitle }}</h1>
          <p>{{ pageSubtitle }}</p>
        </div>
        <div class="token-box">
          <button class="button" :disabled="busy" @click="refreshAdmin"><RefreshCw />刷新</button>
        </div>
      </div>

      <section v-if="activeView === 'accounts'" class="grid">
        <div class="stats">
          <div class="stat"><span>总账号</span><strong>{{ accounts.length }}</strong></div>
          <div class="stat"><span>可分配</span><strong>{{ availableCount }}</strong></div>
          <div class="stat"><span>已兑换</span><strong>{{ redeemedCount }}</strong></div>
          <div class="stat"><span>需要处理</span><strong>{{ attentionCount }}</strong></div>
        </div>

        <div class="grid two">
          <div class="panel">
            <div class="panel-header">
              <h2>账号列表</h2>
              <div class="toolbar">
                <input v-model="filters.search" class="input search-input" placeholder="搜索邮箱 / Account ID" @keyup.enter="loadAccounts" />
                <select v-model="filters.status" class="select status-select" @change="loadAccounts">
                  <option value="">全部状态</option>
                  <option value="available">可用</option>
                  <option value="at_expired">AT 过期</option>
                  <option value="refresh_failed">刷新失败</option>
                  <option value="quota_exhausted">额度耗尽</option>
                  <option value="auth_invalid">账号失效</option>
                  <option value="redeemed">已兑换</option>
                </select>
                <button class="button" @click="loadAccounts"><Search />查询</button>
              </div>
            </div>
            <div class="panel-body">
              <div class="toolbar section-toolbar">
                <button class="button" :disabled="busy" @click="probeSelected"><Activity />测活</button>
                <button class="button" :disabled="busy" @click="refreshSelected"><RotateCcw />刷新 AT</button>
                <select v-model="adminExportFormat" class="select format-select">
                  <option value="cpa">CPA</option>
                  <option value="sub2api">Sub2API</option>
                </select>
                <button class="button primary" :disabled="busy" @click="exportSelected"><Download />导出</button>
              </div>
              <div class="table-wrap">
                <table class="table">
                  <thead>
                    <tr>
                      <th><input type="checkbox" :checked="allSelected" @change="toggleAll" /></th>
                      <th>账号</th>
                      <th>套餐</th>
                      <th>状态</th>
                      <th>AT</th>
                      <th>RT</th>
                      <th>过期</th>
                      <th>测活</th>
                    </tr>
                  </thead>
                  <tbody>
                    <tr v-for="account in accounts" :key="account.id">
                      <td><input v-model="selectedIds" :value="account.id" type="checkbox" /></td>
                      <td>
                        <strong>{{ account.email || account.name || 'Codex Account' }}</strong>
                        <div class="muted mono">{{ account.account_id || account.id }}</div>
                      </td>
                      <td>{{ account.plan_type || '-' }}</td>
                      <td><span class="badge" :class="account.status">{{ statusLabel(account.status) }}</span></td>
                      <td class="mono">{{ account.access_token_preview || '-' }}</td>
                      <td class="mono">{{ account.refresh_token_preview || '-' }}</td>
                      <td>{{ formatTime(account.expires_at) }}</td>
                      <td>{{ formatTime(account.last_probe_at) }}</td>
                    </tr>
                    <tr v-if="!accounts.length"><td colspan="8" class="muted">暂无账号</td></tr>
                  </tbody>
                </table>
              </div>
            </div>
          </div>

          <div class="panel">
            <div class="panel-header">
              <h2>批量导入</h2>
              <button class="button primary" :disabled="busy || !importText.trim()" @click="importAccounts"><Upload />导入</button>
            </div>
            <div class="panel-body">
              <textarea v-model="importText" class="textarea" spellcheck="false" placeholder="粘贴 CPA auth JSON / auth 数组 / Sub2API accounts JSON / Codex token JSONL"></textarea>
              <pre class="result mono admin-result">{{ adminResult || '操作结果会显示在这里。' }}</pre>
            </div>
          </div>
        </div>
      </section>

      <section v-else class="grid two">
        <div class="panel">
          <div class="panel-header"><h2>生成兑换码</h2></div>
          <div class="panel-body grid">
            <input v-model="batchForm.name" class="input" placeholder="批次名称" />
            <input v-model.number="batchForm.total_count" class="input" type="number" min="1" max="5000" placeholder="兑换码数量" />
            <input v-model.number="batchForm.accounts_per_code" class="input" type="number" min="1" max="100" placeholder="每码账号数" />
            <input v-model="batchForm.plan_filter_text" class="input" placeholder="套餐筛选，可选：plus,team" />
            <input v-model="batchForm.expires_at_text" class="input" placeholder="过期时间，可选：2026-07-01T00:00:00+08:00" />
            <button class="button primary" :disabled="busy" @click="createBatch"><Plus />生成</button>
            <pre class="result mono">{{ generatedCodes || '生成后的兑换码会显示在这里。' }}</pre>
          </div>
        </div>

        <div class="panel">
          <div class="panel-header">
            <h2>兑换码批次</h2>
            <button class="button" @click="loadBatches"><RefreshCw />刷新</button>
          </div>
          <div class="panel-body">
            <div class="table-wrap">
              <table class="table">
                <thead><tr><th>批次</th><th>数量</th><th>每码</th><th>状态</th><th></th></tr></thead>
                <tbody>
                  <tr v-for="batch in batches" :key="batch.id">
                    <td><strong>{{ batch.name }}</strong><div class="muted mono">{{ batch.id }}</div></td>
                    <td>{{ batch.redeemed_count }} / {{ batch.total_count }}</td>
                    <td>{{ batch.accounts_per_code }}</td>
                    <td><span class="badge" :class="batch.status">{{ batch.status }}</span></td>
                    <td><button class="button ghost" @click="loadCodes(batch.id)">查看</button></td>
                  </tr>
                  <tr v-if="!batches.length"><td colspan="5" class="muted">暂无批次</td></tr>
                </tbody>
              </table>
            </div>
            <pre class="result mono batch-result">{{ batchCodesText }}</pre>
          </div>
        </div>
      </section>
    </main>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref } from 'vue'
import {
  Activity,
  Database,
  Download,
  FileJson,
  Plus,
  RefreshCw,
  RotateCcw,
  Search,
  Ticket,
  Upload,
} from 'lucide-vue-next'
import { api, type AccountSummary, type EncodedDownload, type ExportFormat, type RedeemBatch, type RedeemCode } from './api/client'

const isAdminRoute = ref(window.location.pathname === '/admin')
const activeView = ref<'accounts' | 'codes'>('accounts')
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
const generatedCodes = ref('')
const redeemText = ref('')
const redeemFormat = ref<ExportFormat>('cpa')
const redeemResult = ref('')
const redeemDocument = ref<unknown | null>(null)
const redeemDownload = ref<EncodedDownload | null>(null)

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
const availableCount = computed(() => accounts.value.filter((item) => item.status === 'available' && !item.redeemed_at).length)
const redeemedCount = computed(() => accounts.value.filter((item) => item.redeemed_at || item.status === 'redeemed').length)
const attentionCount = computed(() => accounts.value.filter((item) => ['at_expired', 'refresh_failed', 'auth_invalid', 'quota_exhausted'].includes(item.status)).length)
const allSelected = computed(() => accounts.value.length > 0 && accounts.value.every((item) => selectedIds.value.includes(item.id)))
const pageTitle = computed(() => activeView.value === 'accounts' ? 'Codex 账号池' : '兑换码管理')
const pageSubtitle = computed(() => activeView.value === 'accounts'
  ? '上传账号、刷新 AT、测活并按 CPA/Sub2API 格式导出'
  : '生成独占兑换码，兑换后账号保留但不再分配')
const batchCodesText = computed(() => batchCodes.value.length ? JSON.stringify(batchCodes.value, null, 2) : '选择批次查看兑换码状态')

async function withBusy(task: () => Promise<void>) {
  busy.value = true
  try {
    await task()
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    if (isAdminRoute.value) adminResult.value = message
    else redeemResult.value = message
  } finally {
    busy.value = false
  }
}

async function loginAdmin() {
  await withBusy(async () => {
    adminResult.value = ''
    adminToken.value = adminTokenDraft.value.trim()
    localStorage.setItem('aether-pool.admin-token', adminToken.value)
    await loadAccounts()
    await loadBatches()
  })
}

function logoutAdmin() {
  adminToken.value = ''
  adminTokenDraft.value = ''
  accounts.value = []
  batches.value = []
  selectedIds.value = []
  localStorage.removeItem('aether-pool.admin-token')
}

async function loadAccounts() {
  const page = await api.listAccounts(apiState.value, filters)
  accounts.value = page.items
  selectedIds.value = selectedIds.value.filter((id) => accounts.value.some((item) => item.id === id))
}

async function loadBatches() {
  const result = await api.listBatches(apiState.value)
  batches.value = result.items
}

async function refreshAdmin() {
  await withBusy(async () => {
    if (activeView.value === 'codes') await loadBatches()
    else await loadAccounts()
  })
}

async function importAccounts() {
  await withBusy(async () => {
    const result = await api.importAccounts(apiState.value, importText.value)
    adminResult.value = JSON.stringify(result, null, 2)
    importText.value = ''
    await loadAccounts()
  })
}

async function probeSelected() {
  await withBusy(async () => {
    const ids = selectedIds.value.length ? selectedIds.value : undefined
    const result = await api.probeAccounts(apiState.value, ids)
    adminResult.value = JSON.stringify(result, null, 2)
    await loadAccounts()
  })
}

async function refreshSelected() {
  await withBusy(async () => {
    const ids = selectedIds.value.length ? selectedIds.value : undefined
    const result = await api.refreshAccounts(apiState.value, ids)
    adminResult.value = JSON.stringify(result, null, 2)
    await loadAccounts()
  })
}

async function exportSelected() {
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

function toggleAll(event: Event) {
  const checked = (event.target as HTMLInputElement).checked
  selectedIds.value = checked ? accounts.value.map((item) => item.id) : []
}

async function createBatch() {
  await withBusy(async () => {
    const expiresAt = batchForm.expires_at_text.trim()
      ? Math.floor(new Date(batchForm.expires_at_text.trim()).getTime() / 1000)
      : null
    const result = await api.createBatch(apiState.value, {
      name: batchForm.name || `兑换码批次 ${new Date().toLocaleString()}`,
      total_count: Number(batchForm.total_count || 1),
      accounts_per_code: Number(batchForm.accounts_per_code || 1),
      expires_at: Number.isFinite(expiresAt) ? expiresAt : null,
      plan_filter: batchForm.plan_filter_text
        .split(',')
        .map((item) => item.trim())
        .filter(Boolean),
    })
    generatedCodes.value = result.codes.map((item) => item.code).join('\n')
    await loadBatches()
  })
}

async function loadCodes(batchId: string) {
  await withBusy(async () => {
    const result = await api.listCodes(apiState.value, batchId)
    batchCodes.value = result.items
  })
}

async function redeemExport() {
  await withBusy(async () => {
    const codes = redeemText.value.split(/\r?\n/).map((item) => item.trim()).filter(Boolean)
    const result = await api.redeemExport({ codes, format: redeemFormat.value })
    redeemDocument.value = result.document
    redeemDownload.value = result.download || null
    redeemResult.value = JSON.stringify(exportResultForDisplay(result), null, 2)
    if (result.download) downloadEncodedFile(result.download)
    else downloadJson(`aether-pool-redeem-${result.format}-${timestamp()}.json`, result.document)
  })
}

function downloadRedeem() {
  if (redeemDownload.value) {
    downloadEncodedFile(redeemDownload.value)
    return
  }
  if (!redeemDocument.value) return
  downloadJson(`aether-pool-redeem-${redeemFormat.value}-${timestamp()}.json`, redeemDocument.value)
}

function exportResultForDisplay<T extends { download?: EncodedDownload | null }>(result: T) {
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

function downloadEncodedFile(download: EncodedDownload) {
  if (download.encoding !== 'base64') return
  const binary = atob(download.data)
  const bytes = new Uint8Array(binary.length)
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index)
  }
  const blob = new Blob([bytes], { type: download.content_type || 'application/octet-stream' })
  downloadBlob(download.filename, blob)
}

function downloadJson(fileName: string, value: unknown) {
  const blob = new Blob([`${JSON.stringify(value, null, 2)}\n`], { type: 'application/json;charset=utf-8' })
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

function timestamp() {
  return new Date().toISOString().replace(/[:.]/g, '-')
}

function statusLabel(status: string) {
  return ({
    available: '可用',
    at_expired: 'AT 过期',
    refresh_failed: '刷新失败',
    auth_invalid: '账号失效',
    quota_exhausted: '额度耗尽',
    redeemed: '已兑换',
  } as Record<string, string>)[status] || status
}

function formatTime(value?: number | null) {
  if (!value) return '-'
  return new Date(value * 1000).toLocaleString()
}

function syncRoute() {
  isAdminRoute.value = window.location.pathname === '/admin'
}

onMounted(() => {
  syncRoute()
  window.addEventListener('popstate', syncRoute)
  if (isAdminRoute.value && adminAuthenticated.value) {
    adminTokenDraft.value = adminToken.value
    loadAccounts().catch((error) => {
      adminResult.value = error instanceof Error ? error.message : String(error)
    })
    loadBatches().catch(() => undefined)
  }
})

onUnmounted(() => {
  window.removeEventListener('popstate', syncRoute)
})
</script>
