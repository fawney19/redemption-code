<template>
  <section class="grid">
    <div class="stats">
      <div class="stat dark-stat">
        <span>总账号</span>
        <strong>{{ accountStats.total }}</strong>
      </div>
      <div class="stat">
        <span>可分配</span>
        <strong>{{ availableCount }}</strong>
      </div>
      <div class="stat">
        <span>已兑换</span>
        <strong>{{ redeemedCount }}</strong>
      </div>
      <div class="stat stat-attention">
        <span>需处理</span>
        <strong>{{ attentionCount }}</strong>
      </div>
    </div>

    <div class="panel accounts-panel">
      <div class="panel-header">
        <div>
          <h2>账号列表</h2>
          <p>批量导入、测活、刷新、导出账号</p>
        </div>
        <div class="account-header-controls">
          <div class="account-header-search">
            <input v-model="filters.search" class="input search-input" placeholder="搜索邮箱 / Account ID" @keyup.enter="searchAccounts" />
          </div>
          <div class="account-command-actions">
            <div class="dropdown-control account-bulk-menu-control">
              <button
                class="button"
                :class="{ active: bulkMenuOpen }"
                :disabled="busy"
                :aria-expanded="bulkMenuOpen"
                @click="toggleBulkMenu"
              >
                {{ selectedIds.length ? `批量操作 (${selectedIds.length})` : '批量操作' }} <span class="button-caret" aria-hidden="true"></span>
              </button>
              <div v-if="bulkMenuOpen" class="action-dropdown-menu" role="menu">
                <button type="button" role="menuitem" :disabled="busy || !selectedIds.length" @click="runBulkProbe">
                  <Activity :size="14" />测活选中
                </button>
                <button type="button" role="menuitem" :disabled="busy || !selectedIds.length" @click="runBulkRefresh">
                  <RotateCcw :size="14" />刷新 AT
                </button>
                <button type="button" role="menuitem" class="danger-menu-item" :disabled="busy || !selectedIds.length" @click="runBulkDelete">
                  <Trash2 :size="14" />删除选中
                </button>
              </div>
            </div>
            <button class="button" :class="{ active: importPanelOpen }" :disabled="busy" @click="importPanelOpen = !importPanelOpen">
              <Upload :size="15" />批量导入
            </button>
            <div class="dropdown-control account-export-menu-control">
              <button
                class="button primary"
                :class="{ active: exportMenuOpen }"
                :disabled="busy"
                :title="selectedIds.length ? '导出选中的账号' : '导出当前号池未兑换库存'"
                :aria-expanded="exportMenuOpen"
                @click="toggleExportMenu"
              >
                <Download :size="15" />导出 <span class="button-caret" aria-hidden="true"></span>
              </button>
              <div v-if="exportMenuOpen" class="action-dropdown-menu export-dropdown-menu" role="menu">
                <button type="button" role="menuitem" :disabled="busy" @click="exportWithFormat('cpa')">
                  {{ selectedIds.length ? '导出选中 CPA' : '导出库存 CPA' }}
                </button>
                <button type="button" role="menuitem" :disabled="busy" @click="exportWithFormat('sub2api')">
                  {{ selectedIds.length ? '导出选中 Sub2API' : '导出库存 Sub2API' }}
                </button>
              </div>
            </div>
          </div>
        </div>
      </div>
      <div class="panel-body">
        <Transition name="fade">
          <div v-if="importPanelOpen" class="inline-import-panel">
            <div class="settings-grid">
              <label class="field-label">
                <span>导入到号池</span>
                <select v-model="importPoolId" class="select">
                  <option v-for="pool in activePools" :key="pool.id" :value="pool.id">
                    {{ poolLabel(pool.id) }}
                  </option>
                </select>
              </label>
              <div class="inline-import-actions">
                <button class="button primary" :disabled="busy || !importText.trim()" @click="importAccounts">
                  <Upload :size="15" />导入
                </button>
                <button class="button ghost" :disabled="busy" @click="importPanelOpen = false">收起</button>
              </div>
            </div>
            <textarea v-model="importText" class="textarea import-textarea" spellcheck="false" placeholder="粘贴 CPA auth JSON / auth 数组 / Sub2API accounts JSON / Codex token JSONL"></textarea>
            <Transition name="fade">
              <pre v-if="adminResult" class="result mono admin-result">{{ adminResult }}</pre>
            </Transition>
          </div>
        </Transition>
        <div class="table-wrap">
          <table class="table">
            <thead>
              <tr>
                <th><input type="checkbox" :checked="allSelected" @change="toggleAll" /></th>
                <th>账号</th>
                <th>
                  <div class="table-filter-heading">
                    <div class="table-filter-control">
                      <button
                        class="table-filter-button"
                        :class="{ active: filters.pool_ids.length }"
                        type="button"
                        title="筛选号池"
                        aria-label="筛选号池"
                        :aria-expanded="headerFilterMenu === 'pool'"
                        @click="toggleHeaderFilterMenu('pool')"
                      >
                        <ListFilter :size="15" />
                      </button>
                      <div v-if="headerFilterMenu === 'pool'" class="table-filter-menu">
                        <div class="filter-menu-title">号池</div>
                        <label v-for="pool in accountPools" :key="pool.id" class="filter-option pool-filter-option">
                          <input type="checkbox" :checked="filters.pool_ids.includes(pool.id)" @change="togglePoolFilter(pool.id)" />
                          <span class="filter-option-content">
                            <strong>{{ pool.name }}</strong>
                            <small v-if="poolMetaLabel(pool)">{{ poolMetaLabel(pool) }}</small>
                          </span>
                        </label>
                        <button v-if="filters.pool_ids.length" class="filter-clear-button" type="button" @click="clearPoolFilter">清除号池筛选</button>
                      </div>
                    </div>
                    <span>号池</span>
                  </div>
                </th>
                <th>
                  <div class="table-filter-heading">
                    <div class="table-filter-control">
                      <button
                        class="table-filter-button"
                        :class="{ active: filters.statuses.length || filters.redeemed_values.length }"
                        type="button"
                        title="筛选状态 / 兑换"
                        aria-label="筛选状态 / 兑换"
                        :aria-expanded="headerFilterMenu === 'state'"
                        @click="toggleHeaderFilterMenu('state')"
                      >
                        <ListFilter :size="15" />
                      </button>
                      <div v-if="headerFilterMenu === 'state'" class="table-filter-menu state-filter-menu">
                        <div class="filter-menu-title">账号状态</div>
                        <label v-for="option in statusFilterOptions" :key="option.value" class="filter-option">
                          <input type="checkbox" :checked="filters.statuses.includes(option.value)" @change="toggleStatusFilter(option.value)" />
                          <span>{{ option.label }}</span>
                        </label>
                        <div class="filter-menu-title separated">兑换状态</div>
                        <label v-for="option in redeemedFilterOptions" :key="option.value" class="filter-option">
                          <input type="checkbox" :checked="filters.redeemed_values.includes(option.value)" @change="toggleRedeemedFilter(option.value)" />
                          <span>{{ option.label }}</span>
                        </label>
                        <button
                          v-if="filters.statuses.length || filters.redeemed_values.length"
                          class="filter-clear-button"
                          type="button"
                          @click="clearStateFilter"
                        >
                          清除状态筛选
                        </button>
                      </div>
                    </div>
                    <span>状态 / 兑换</span>
                  </div>
                </th>
                <th>额度</th>
                <th>时间</th>
                <th>测活</th>
                <th>操作</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="account in accounts" :key="account.id">
                <td><input v-model="selectedIds" :value="account.id" type="checkbox" /></td>
                <td>
                  <strong>{{ account.email || account.name || 'Codex Account' }}</strong>
                  <div class="muted mono">{{ account.account_id || account.id }}</div>
                </td>
                <td>
                  <span class="badge available">{{ account.pool_name || poolLabel(account.pool_id) }}</span>
                  <div class="muted mono">{{ account.pool_id }}</div>
                </td>
                <td>
                  <div class="account-state-stack">
                    <div class="badge-row">
                      <span class="badge redeem-state-badge" :class="account.redeemed_at || account.redeem_code_id ? 'redeemed' : 'available'">
                        {{ account.redeemed_at || account.redeem_code_id ? '已兑换' : '未兑换' }}
                      </span>
                      <span class="badge" :class="statusBadgeClass(account.status)">{{ statusLabel(account.status) }}</span>
                    </div>
                    <div class="muted mono redeem-code-line">{{ account.redeem_code_masked || '未绑定兑换码' }}</div>
                  </div>
                </td>
                <td>
                  <div class="quota-stack">
                    <div class="quota-row" :class="quotaUsageClass(account, 'five_hour')">
                      <span class="quota-label">5h</span>
                      <span class="quota-track">
                        <span class="quota-fill" :style="{ width: quotaPercentBarWidth(account, 'five_hour') }"></span>
                      </span>
                      <span class="quota-value">{{ quotaPercentText(account, 'five_hour') }}</span>
                    </div>
                    <div class="quota-row" :class="quotaUsageClass(account, 'weekly')">
                      <span class="quota-label">周</span>
                      <span class="quota-track">
                        <span class="quota-fill" :style="{ width: quotaPercentBarWidth(account, 'weekly') }"></span>
                      </span>
                      <span class="quota-value">{{ quotaPercentText(account, 'weekly') }}</span>
                    </div>
                  </div>
                </td>
                <td>
                  <div class="account-time-stack">
                    <span><b>过期</b>{{ formatTime(account.expires_at) }}</span>
                    <span><b>兑换</b>{{ formatTime(account.redeemed_at) }}</span>
                  </div>
                </td>
                <td>{{ formatTime(account.last_probe_at) }}</td>
                <td>
                  <div class="account-actions">
                    <button
                      class="button ghost tiny icon-only"
                      :disabled="busy"
                      title="测活"
                      aria-label="测活"
                      @click="probeAccount(account.id)"
                    >
                      <Activity :size="14" />
                    </button>
                    <button
                      class="button ghost tiny icon-only"
                      :disabled="busy"
                      title="刷新 AT"
                      aria-label="刷新 AT"
                      @click="refreshAccountToken(account)"
                    >
                      <RotateCcw :size="14" />
                    </button>
                    <div class="token-copy-control">
                      <button
                        class="button ghost tiny icon-only"
                        :class="{ active: tokenMenuAccountId === account.id }"
                        :disabled="busy"
                        title="复制 AT / RT"
                        aria-label="复制 AT / RT"
                        :aria-expanded="tokenMenuAccountId === account.id"
                        @click="toggleTokenMenu(account.id)"
                      >
                        <Copy :size="14" />
                      </button>
                      <div v-if="tokenMenuAccountId === account.id" class="token-copy-menu" role="menu">
                        <button type="button" role="menuitem" :disabled="busy" @click="copyAccountToken(account, 'access')">复制 AT</button>
                        <button type="button" role="menuitem" :disabled="busy" @click="copyAccountToken(account, 'refresh')">复制 RT</button>
                      </div>
                    </div>
                    <button
                      class="button ghost tiny icon-only"
                      :disabled="busy"
                      title="导出账号"
                      aria-label="导出账号"
                      @click="exportAccount(account)"
                    >
                      <Download :size="14" />
                    </button>
                    <button
                      class="button ghost danger borderless tiny icon-only"
                      :disabled="busy || !canDeleteAccount(account)"
                      :title="canDeleteAccount(account) ? '删除账号' : '已兑换账号需测活为失效状态后才能删除'"
                      :aria-label="canDeleteAccount(account) ? '删除账号' : '已兑换账号需测活为失效状态后才能删除'"
                      @click="deleteAccount(account.id)"
                    >
                      <Trash2 :size="14" />
                    </button>
                  </div>
                </td>
              </tr>
              <tr v-if="!accounts.length">
                <td colspan="8" class="empty-row">
                  <Database :size="20" />
                  <span>暂无账号数据</span>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <div class="account-pagination">
          <div class="pagination-summary">
            <strong>{{ accountPageStart }}-{{ accountPageEnd }}</strong>
            <span>/ {{ accountPagination.total }} 个账号</span>
          </div>
          <div class="pagination-controls">
            <label>
              <span>每页</span>
              <select v-model.number="accountPagination.limit" class="select page-size-select" @change="changeAccountsPageSize">
                <option :value="25">25</option>
                <option :value="50">50</option>
                <option :value="100">100</option>
                <option :value="200">200</option>
              </select>
            </label>
            <span class="pagination-page">第 {{ accountCurrentPage }} / {{ accountTotalPages }} 页</span>
            <button class="button ghost tiny" :disabled="busy || !canPrevAccountsPage" @click="previousAccountsPage">上一页</button>
            <button class="button ghost tiny" :disabled="busy || !canNextAccountsPage" @click="nextAccountsPage">下一页</button>
          </div>
        </div>
      </div>
    </div>
  </section>
</template>

<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue'
import { Activity, Copy, Database, Download, ListFilter, RotateCcw, Trash2, Upload } from 'lucide-vue-next'
import { api, type AccountPool, type AccountSummary, type ExportFormat } from '../api/client'
import {
  useAdmin,
  importAccounts,
  searchAccounts,
  previousAccountsPage,
  nextAccountsPage,
  changeAccountsPageSize,
  probeSelected,
  probeAccount,
  refreshSelected,
  deleteSelectedAccounts,
  deleteAccount,
  exportSelected,
  loadAccounts,
  toggleAll,
  downloadEncodedFile,
  downloadJson,
  exportResultForDisplay,
  poolLabel,
  statusLabel,
  statusBadgeClass,
  canDeleteAccount,
  quotaPercentText,
  quotaPercentBarWidth,
  quotaUsageClass,
  formatTime,
  timestamp,
} from '../composables/useAdmin'
import { useToast } from '../composables/useToast'

const {
  accounts,
  selectedIds,
  importText,
  accountPools,
  activePools,
  importPoolId,
  adminResult,
  adminExportFormat,
  filters,
  accountPagination,
  accountStats,
  busy,
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
  apiState,
} = useAdmin()

const importPanelOpen = ref(false)
const tokenMenuAccountId = ref('')
const bulkMenuOpen = ref(false)
const exportMenuOpen = ref(false)
const headerFilterMenu = ref<'pool' | 'state' | ''>('')
const toast = useToast()
const statusFilterOptions = [
  { value: 'available', label: '可用' },
  { value: 'at_expired', label: 'AT 过期' },
  { value: 'refresh_failed', label: '刷新失败' },
  { value: 'quota_exhausted', label: '额度耗尽' },
  { value: 'auth_invalid', label: '账号失效' },
  { value: 'forbidden', label: '网络受限' },
]
const redeemedFilterOptions = [
  { value: 'false', label: '未兑换' },
  { value: 'true', label: '已兑换' },
]

function poolMetaLabel(pool: AccountPool) {
  return [pool.workspace_label, pool.account_type].filter(Boolean).join(' / ')
}

onMounted(() => {
  document.addEventListener('click', closeTokenMenuOnOutsideClick)
})

onBeforeUnmount(() => {
  document.removeEventListener('click', closeTokenMenuOnOutsideClick)
})

function toggleTokenMenu(accountId: string) {
  tokenMenuAccountId.value = tokenMenuAccountId.value === accountId ? '' : accountId
  bulkMenuOpen.value = false
  exportMenuOpen.value = false
  headerFilterMenu.value = ''
}

function toggleBulkMenu() {
  bulkMenuOpen.value = !bulkMenuOpen.value
  exportMenuOpen.value = false
  tokenMenuAccountId.value = ''
  headerFilterMenu.value = ''
}

function toggleExportMenu() {
  exportMenuOpen.value = !exportMenuOpen.value
  bulkMenuOpen.value = false
  tokenMenuAccountId.value = ''
  headerFilterMenu.value = ''
}

function toggleHeaderFilterMenu(menu: 'pool' | 'state') {
  headerFilterMenu.value = headerFilterMenu.value === menu ? '' : menu
  tokenMenuAccountId.value = ''
  bulkMenuOpen.value = false
  exportMenuOpen.value = false
}

function closeTokenMenuOnOutsideClick(event: MouseEvent) {
  if (!tokenMenuAccountId.value && !bulkMenuOpen.value && !exportMenuOpen.value && !headerFilterMenu.value) return
  const target = event.target
  if (target instanceof Element && target.closest('.token-copy-control')) return
  if (target instanceof Element && target.closest('.dropdown-control')) return
  if (target instanceof Element && target.closest('.table-filter-control')) return
  tokenMenuAccountId.value = ''
  bulkMenuOpen.value = false
  exportMenuOpen.value = false
  headerFilterMenu.value = ''
}

function toggleArrayValue(values: string[], value: string) {
  const index = values.indexOf(value)
  if (index >= 0) values.splice(index, 1)
  else values.push(value)
}

async function togglePoolFilter(poolId: string) {
  toggleArrayValue(filters.pool_ids, poolId)
  await searchAccounts()
}

async function toggleStatusFilter(status: string) {
  toggleArrayValue(filters.statuses, status)
  await searchAccounts()
}

async function toggleRedeemedFilter(value: string) {
  toggleArrayValue(filters.redeemed_values, value)
  await searchAccounts()
}

async function clearPoolFilter() {
  filters.pool_ids = []
  await searchAccounts()
}

async function clearStateFilter() {
  filters.statuses = []
  filters.redeemed_values = []
  await searchAccounts()
}

async function runBulkProbe() {
  if (!selectedIds.value.length) return
  bulkMenuOpen.value = false
  await probeSelected()
}

async function runBulkRefresh() {
  if (!selectedIds.value.length) return
  bulkMenuOpen.value = false
  await refreshSelected()
}

async function runBulkDelete() {
  if (!selectedIds.value.length) return
  bulkMenuOpen.value = false
  await deleteSelectedAccounts()
}

async function exportWithFormat(format: ExportFormat) {
  adminExportFormat.value = format
  exportMenuOpen.value = false
  await exportSelected()
}

async function copyAccountToken(account: AccountSummary, kind: 'access' | 'refresh') {
  try {
    const result = await api.getAccountToken(apiState.value, account.id, kind)
    await copyText(result.token)
    tokenMenuAccountId.value = ''
    toast.success(`已复制 ${kind === 'access' ? 'AT' : 'RT'}`)
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    toast.error(message || '复制失败')
  }
}

async function refreshAccountToken(account: AccountSummary) {
  busy.value = true
  try {
    const result = await api.refreshAccounts(apiState.value, [account.id])
    await loadAccounts()
    toast.success(result.refreshed > 0 ? 'AT 已刷新' : '刷新完成')
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    toast.error(message || '刷新失败')
  } finally {
    busy.value = false
  }
}

async function exportAccount(account: AccountSummary) {
  busy.value = true
  try {
    const result = await api.exportAccounts(apiState.value, {
      account_ids: [account.id],
      include_redeemed: true,
      format: adminExportFormat.value,
    })
    adminResult.value = JSON.stringify(exportResultForDisplay(result), null, 2)
    if (result.download) downloadEncodedFile(result.download)
    else downloadJson(`account-pool-account-${account.id}-${result.format}-${timestamp()}.json`, result.document)
    toast.success('账号已导出')
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    toast.error(message || '导出失败')
  } finally {
    busy.value = false
  }
}

async function copyText(value: string) {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(value)
    return
  }
  const textarea = document.createElement('textarea')
  textarea.value = value
  textarea.style.position = 'fixed'
  textarea.style.left = '-9999px'
  document.body.append(textarea)
  textarea.focus()
  textarea.select()
  const copied = document.execCommand('copy')
  textarea.remove()
  if (!copied) throw new Error('当前浏览器不允许自动复制')
}
</script>
