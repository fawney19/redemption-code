<template>
  <section class="grid accounts-view">
    <template v-if="!selectedPoolId">
      <div class="stats">
        <div class="stat dark-stat">
          <span>总号池</span>
          <strong>{{ accountPools.length }}</strong>
        </div>
        <div class="stat">
          <span>总账号</span>
          <strong>{{ poolOverviewStats.total }}</strong>
        </div>
        <div class="stat">
          <span>可分配</span>
          <strong>{{ poolOverviewStats.available }}</strong>
        </div>
        <div class="stat stat-attention">
          <span>需处理</span>
          <strong>{{ poolOverviewStats.attention }}</strong>
        </div>
      </div>

      <div class="panel account-pool-overview-panel">
        <div class="panel-header">
          <div>
            <h2>选择号池</h2>
            <p>进入分号池后查看账号、筛选并批量处理</p>
          </div>
          <button class="button" :disabled="busy" @click="loadPools">
            <RefreshCw :size="15" :class="{ spinning: busy }" />刷新
          </button>
        </div>
        <div class="panel-body">
          <div class="account-pool-grid">
            <button v-for="pool in accountPools" :key="pool.id" type="button" class="account-pool-card" @click="openPool(pool)">
              <span class="account-pool-card-top">
                <span>
                  <strong>{{ pool.name }}</strong>
                  <small class="mono">{{ pool.id }}</small>
                </span>
                <span class="pool-status-stack">
                  <span v-if="pool.is_default" class="badge available">默认</span>
                  <span class="badge" :class="pool.is_active ? 'available' : 'disabled'">
                    {{ pool.is_active ? '启用' : '停用' }}
                  </span>
                </span>
              </span>
              <span class="account-pool-card-meta">{{ poolMetaLabel(pool) || 'codex' }}</span>
              <span class="account-pool-card-stats">
                <span><b>{{ pool.stats.total }}</b>总账号</span>
                <span><b>{{ pool.stats.available }}</b>可分配</span>
                <span><b>{{ pool.stats.redeemed }}</b>已兑换</span>
                <span :class="{ attention: pool.stats.attention }"><b>{{ pool.stats.attention }}</b>需处理</span>
              </span>
            </button>
          </div>
          <div v-if="!accountPools.length" class="empty-row account-pool-empty">
            <Database :size="20" />
            <span>暂无号池</span>
          </div>
        </div>
      </div>
    </template>

    <template v-else-if="selectedPool">
      <div class="account-pool-detail-bar">
        <button class="button ghost" :disabled="busy" @click="leaveAccountPool">
          <ChevronLeft :size="15" />返回号池
        </button>
        <div>
          <h2>{{ selectedPool.name }}</h2>
          <p>
            <span class="mono">{{ selectedPool.id }}</span>
            <span v-if="poolMetaLabel(selectedPool)"> · {{ poolMetaLabel(selectedPool) }}</span>
            <span> · {{ selectedPool.is_active ? '启用中' : '已停用' }}</span>
          </p>
        </div>
      </div>

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
                <div v-if="bulkMenuOpen" class="action-dropdown-menu bulk-action-menu" role="menu">
                  <div class="action-menu-section-title">选中账号</div>
                  <button type="button" role="menuitem" :disabled="busy || !selectedIds.length" @click="runBulkProbeSelected">
                    <Activity :size="14" />测活选中
                  </button>
                  <button type="button" role="menuitem" :disabled="busy || !selectedIds.length" @click="runBulkRefreshSelected">
                    <RotateCcw :size="14" />刷新选中
                  </button>
                  <button type="button" role="menuitem" class="danger-menu-item" :disabled="busy || !selectedIds.length" @click="runBulkDeleteSelected">
                    <Trash2 :size="14" />删除选中
                  </button>
                  <div class="action-menu-section-title">当前筛选范围</div>
                  <button type="button" role="menuitem" :disabled="busy || !bulkFilterTargetCount" @click="runBulkProbeFiltered">
                    <Activity :size="14" />测活筛选 ({{ bulkFilterTargetCount }})
                  </button>
                  <button type="button" role="menuitem" :disabled="busy || !bulkFilterTargetCount" @click="runBulkRefreshFiltered">
                    <RotateCcw :size="14" />刷新筛选 ({{ bulkFilterTargetCount }})
                  </button>
                  <button type="button" role="menuitem" class="danger-menu-item" :disabled="busy || !bulkFilterTargetCount" @click="runBulkDeleteFiltered">
                    <Trash2 :size="14" />删除筛选 ({{ bulkFilterTargetCount }})
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
              <div class="inline-import-header">
                <div>
                  <span>导入到号池</span>
                  <strong>{{ selectedPool.name }}</strong>
                </div>
                <span v-if="!selectedPool.is_active" class="badge disabled">已停用</span>
              </div>
              <div class="inline-import-actions">
                <button class="button primary" :disabled="busy || !importText.trim() || !selectedPool.is_active" @click="importAccounts">
                  <Upload :size="15" />导入
                </button>
                <button class="button ghost" :disabled="busy" @click="importPanelOpen = false">收起</button>
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
                  <td colspan="7" class="empty-row">
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
    </template>

    <div v-else class="panel">
      <div class="panel-body account-missing-pool">
        <Database :size="24" />
        <strong>号池不存在</strong>
        <button class="button" @click="leaveAccountPool">
          <ChevronLeft :size="15" />返回号池
        </button>
      </div>
    </div>
  </section>
</template>

<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue'
import { Activity, ChevronLeft, Copy, Database, Download, ListFilter, RefreshCw, RotateCcw, Trash2, Upload } from 'lucide-vue-next'
import { api, type AccountPool, type AccountSummary, type ExportFormat } from '../api/client'
import {
  useAdmin,
  importAccounts,
  searchAccounts,
  previousAccountsPage,
  nextAccountsPage,
  changeAccountsPageSize,
  enterAccountPool,
  leaveAccountPool,
  probeSelected,
  probeFilteredAccounts,
  probeAccount,
  refreshSelected,
  refreshFilteredAccounts,
  deleteSelectedAccounts,
  deleteFilteredAccounts,
  deleteAccount,
  exportSelected,
  loadAccounts,
  loadPools,
  toggleAll,
  downloadEncodedFile,
  downloadJson,
  exportResultForDisplay,
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
  selectedPoolId,
  selectedPool,
  poolOverviewStats,
  bulkFilterTargetCount,
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
const headerFilterMenu = ref<'state' | ''>('')
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
  document.addEventListener('click', closeMenusOnOutsideClick)
})

onBeforeUnmount(() => {
  document.removeEventListener('click', closeMenusOnOutsideClick)
})

async function openPool(pool: AccountPool) {
  await enterAccountPool(pool.id)
}

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

function toggleHeaderFilterMenu(menu: 'state') {
  headerFilterMenu.value = headerFilterMenu.value === menu ? '' : menu
  tokenMenuAccountId.value = ''
  bulkMenuOpen.value = false
  exportMenuOpen.value = false
}

function closeMenusOnOutsideClick(event: MouseEvent) {
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

async function toggleStatusFilter(status: string) {
  toggleArrayValue(filters.statuses, status)
  await searchAccounts()
}

async function toggleRedeemedFilter(value: string) {
  toggleArrayValue(filters.redeemed_values, value)
  await searchAccounts()
}

async function clearStateFilter() {
  filters.statuses = []
  filters.redeemed_values = []
  await searchAccounts()
}

async function runBulkProbeSelected() {
  if (!selectedIds.value.length) return
  bulkMenuOpen.value = false
  await probeSelected()
}

async function runBulkRefreshSelected() {
  if (!selectedIds.value.length) return
  bulkMenuOpen.value = false
  await refreshSelected()
}

async function runBulkDeleteSelected() {
  if (!selectedIds.value.length) return
  bulkMenuOpen.value = false
  await deleteSelectedAccounts()
}

async function runBulkProbeFiltered() {
  if (!bulkFilterTargetCount.value) return
  bulkMenuOpen.value = false
  await probeFilteredAccounts()
}

async function runBulkRefreshFiltered() {
  if (!bulkFilterTargetCount.value) return
  bulkMenuOpen.value = false
  await refreshFilteredAccounts()
}

async function runBulkDeleteFiltered() {
  if (!bulkFilterTargetCount.value) return
  bulkMenuOpen.value = false
  await deleteFilteredAccounts()
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
    await loadPools()
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
