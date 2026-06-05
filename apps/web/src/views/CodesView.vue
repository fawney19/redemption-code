<template>
  <section class="codes-view grid">
    <div class="stats codes-stats">
      <div class="stat dark-stat">
        <span>兑换码总数</span>
        <strong>{{ redeemStats.totalCodes }}</strong>
      </div>
      <div class="stat">
        <span>已兑换</span>
        <strong>{{ redeemStats.redeemedCodes }}</strong>
      </div>
      <div class="stat">
        <span>未兑换</span>
        <strong>{{ redeemStats.availableCodes }}</strong>
      </div>
      <div class="stat stat-attention">
        <span>兑换率</span>
        <strong>{{ redeemStats.redemptionRate }}%</strong>
      </div>
    </div>

    <section class="admin-grid codes-grid">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h2>生成兑换码</h2>
            <p>独占分配账号</p>
          </div>
        </div>
        <div class="panel-body grid">
          <label class="field-label full">
            <span>号池</span>
            <select v-model="batchForm.pool_id" class="select">
              <option v-for="pool in activePools" :key="pool.id" :value="pool.id">
                {{ poolLabel(pool.id) }}
              </option>
            </select>
          </label>
          <input v-model="batchForm.name" class="input" placeholder="批次名称" />
          <input v-model.number="batchForm.total_count" class="input" type="number" min="1" max="5000" placeholder="兑换码数量" />
          <input v-model.number="batchForm.accounts_per_code" class="input" type="number" min="1" max="100" placeholder="每码账号数" />
          <input v-model.number="batchForm.after_sale_limit" class="input" type="number" min="0" max="10" placeholder="每码售后次数" />
          <input v-model="batchForm.expires_at_text" class="input" placeholder="过期时间，可选：2026-07-01T00:00:00+08:00" />
          <button class="button primary" :disabled="busy" @click="createBatch">
            <Plus :size="15" />生成
          </button>
          <Transition name="fade">
            <div v-if="generatedCodes" class="generated-codes-wrap">
              <div class="generated-codes-toolbar">
                <div>
                  <strong>本次生成 {{ generatedCodeCount }} 个完整兑换码</strong>
                  <span>完整兑换码会加密存储，可在批次中再次复制或导出</span>
                </div>
                <div class="toolbar compact-toolbar">
                  <button class="button ghost" @click="copyGeneratedCodes">
                    <Copy :size="15" />复制
                  </button>
                  <button class="button ghost" @click="exportGeneratedCodes">
                    <Download :size="15" />导出 TXT
                  </button>
                </div>
              </div>
              <div class="terminal-bar"><span></span><span></span><span></span></div>
              <pre class="result mono dark-result generated-codes-result">{{ generatedCodes }}</pre>
            </div>
          </Transition>
        </div>
      </div>

      <div class="panel">
        <div class="panel-header">
          <div>
            <h2>兑换码批次</h2>
            <p>复制、导出、查看兑换状态</p>
          </div>
          <div class="toolbar compact-toolbar">
            <button class="button" :disabled="!selectedBatch" @click="copySelectedBatchCodes">
              <Copy :size="15" />复制
            </button>
            <button class="button" :disabled="!selectedBatch" @click="exportSelectedBatchCodes">
              <Download :size="15" />导出 CSV
            </button>
            <button class="button" @click="loadBatches">
              <RefreshCw :size="15" />刷新
            </button>
          </div>
        </div>
        <div class="panel-body grid">
          <div class="redeem-summary">
            <div>
              <span>批次数</span>
              <strong>{{ redeemStats.totalBatches }}</strong>
            </div>
            <div>
              <span>启用批次</span>
              <strong>{{ redeemStats.activeBatches }}</strong>
            </div>
            <div>
              <span>过期批次</span>
              <strong>{{ redeemStats.expiredBatches }}</strong>
            </div>
            <div>
              <span>已分配账号</span>
              <strong>{{ redeemStats.redeemedAccounts }}</strong>
            </div>
          </div>

          <div class="table-wrap">
            <table class="table batch-table">
              <thead>
                <tr>
                  <th>批次</th>
                  <th>号池</th>
                  <th>兑换进度</th>
                  <th>每码</th>
                  <th>售后</th>
                  <th>状态</th>
                  <th>操作</th>
                </tr>
              </thead>
              <tbody>
                <tr
                  v-for="batch in batches"
                  :key="batch.id"
                  :class="{ selected: batch.id === selectedBatchId }"
                >
                  <td>
                    <strong>{{ batch.name }}</strong>
                    <div class="muted mono">{{ batch.id }}</div>
                  </td>
                  <td>
                    <span class="badge available">{{ batch.pool_name || poolLabel(batch.pool_id) }}</span>
                    <div class="muted mono">{{ batch.pool_id }}</div>
                  </td>
                  <td>
                    <div class="progress-cell">
                      <span>{{ batch.redeemed_count }} / {{ batch.total_count }}</span>
                      <div class="progress-track">
                        <div class="progress-fill" :style="{ width: `${batchRate(batch)}%` }"></div>
                      </div>
                    </div>
                  </td>
                  <td>{{ batch.accounts_per_code }} 账号</td>
                  <td>{{ batch.after_sale_limit }} 次</td>
                  <td>
                    <span class="badge" :class="batchStatusClass(batch)">{{ batchStatusLabel(batch) }}</span>
                  </td>
                  <td>
                    <div class="row-actions">
                      <button class="button ghost tiny" @click="selectBatch(batch.id)">查看</button>
                      <button class="button ghost tiny" @click="copyBatchCodes(batch)">复制</button>
                      <button class="button ghost tiny" @click="exportBatchCodes(batch)">导出 CSV</button>
                      <button class="button danger tiny" :disabled="busy" @click="deleteBatch(batch)">
                        <Trash2 :size="14" />删除
                      </button>
                    </div>
                  </td>
                </tr>
                <tr v-if="!batches.length">
                  <td colspan="7" class="empty-row">
                    <Ticket :size="20" />
                    <span>暂无批次</span>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>

          <div class="code-detail-panel">
            <div class="code-detail-header">
              <div>
                <h3>{{ selectedBatch ? selectedBatch.name : '兑换状态' }}</h3>
                <p>{{ selectedBatch ? selectedBatch.id : '选择一个批次查看单码状态' }}</p>
              </div>
              <div class="detail-stats">
                <span>{{ selectedBatchStats.redeemedCodes }} 已兑</span>
                <span>{{ selectedBatchStats.activeCodes }} 可兑</span>
                <span>{{ selectedBatchStats.afterSaleCount }} 售后</span>
                <span>{{ selectedBatchStats.redemptionRate }}%</span>
              </div>
            </div>

            <div v-if="selectedBatch" class="table-wrap detail-table-wrap">
              <table class="table detail-table">
                <thead>
                  <tr>
                    <th>兑换码</th>
                    <th>状态</th>
                    <th>绑定账号</th>
                    <th>兑换时间</th>
                    <th>兑换记录</th>
                    <th>售后</th>
                    <th>操作</th>
                  </tr>
                </thead>
                <tbody>
                  <tr v-for="code in batchCodes" :key="code.id">
                    <td>
                      <strong class="mono">{{ displayCode(code) }}</strong>
                      <div v-if="!code.code" class="muted">历史脱敏</div>
                    </td>
                    <td><span class="badge" :class="code.status">{{ statusLabel(code.status) }}</span></td>
                    <td>
                      <div v-if="code.accounts.length" class="bound-account-list">
                        <div v-for="account in code.accounts" :key="account.id" class="bound-account-item">
                          <div>
                            <strong>{{ accountDisplayName(account) }}</strong>
                            <span class="muted mono">{{ accountDetailText(account) }}</span>
                          </div>
                          <span class="badge" :class="statusBadgeClass(account.status)">{{ statusLabel(account.status) }}</span>
                        </div>
                      </div>
                      <span v-else class="muted">-</span>
                    </td>
                    <td>{{ formatTime(code.redeemed_at) }}</td>
                    <td class="mono">{{ code.redemption_id || '-' }}</td>
                    <td>
                      <div class="after-sale-cell">
                        <span class="badge" :class="code.after_sale_count ? 'redeemed' : 'disabled'">
                          {{ code.after_sale_count }} 次
                        </span>
                        <div v-if="code.after_sales.length" class="after-sale-list">
                          <div v-for="afterSale in code.after_sales" :key="afterSale.id" class="after-sale-item">
                            <strong>{{ formatTime(afterSale.created_at) }}</strong>
                            <span>{{ afterSale.reason || statusLabel(afterSale.status) }}</span>
                            <small>{{ formatAfterSaleAccounts(afterSale) }}</small>
                          </div>
                        </div>
                      </div>
                    </td>
                    <td>
                      <button
                        class="button ghost tiny"
                        :disabled="busy || probingCodeId === code.id || !boundAccountIds(code).length"
                        @click="probeCodeAccounts(code)"
                      >
                        <Activity :size="14" />测活
                      </button>
                    </td>
                  </tr>
                  <tr v-if="!batchCodes.length">
                    <td colspan="7" class="empty-row">
                      <Ticket :size="20" />
                      <span>暂无兑换码明细</span>
                    </td>
                  </tr>
                </tbody>
              </table>
            </div>
          </div>

          <Transition name="fade">
            <div v-if="manualCopyText" class="manual-copy-panel">
              <div class="manual-copy-header">
                <div>
                  <strong>{{ manualCopyTitle }}</strong>
                  <span>当前浏览器限制自动写入剪贴板，可手动选择复制。</span>
                </div>
                <button class="button ghost tiny" @click="manualCopyText = ''">关闭</button>
              </div>
              <textarea
                class="textarea mono manual-copy-textarea"
                readonly
                :value="manualCopyText"
                @focus="selectManualCopyText"
              ></textarea>
            </div>
          </Transition>
        </div>
      </div>
    </section>
  </section>
</template>

<script setup lang="ts">
import { computed, ref } from 'vue'
import { Activity, Copy, Download, Plus, RefreshCw, Ticket, Trash2 } from 'lucide-vue-next'
import type { RedeemAfterSale, RedeemBatch, RedeemCode } from '../api/client'
import { api } from '../api/client'
import {
  useAdmin,
  createBatch,
  deleteBatch,
  loadBatches,
  loadCodes,
  downloadText,
  formatTime,
  statusLabel,
  statusBadgeClass,
  poolLabel,
  timestamp,
} from '../composables/useAdmin'
import { useToast } from '../composables/useToast'

const {
  batches,
  batchForm,
  batchCodes,
  selectedBatchId,
  selectedBatch,
  selectedBatchStats,
  generatedCodes,
  redeemStats,
  activePools,
  busy,
  apiState,
} = useAdmin()
const toast = useToast()
const manualCopyTitle = ref('')
const manualCopyText = ref('')
const probingCodeId = ref('')

const generatedCodeCount = computed(() => generatedCodes.value.split(/\r?\n/).filter(Boolean).length)
const selectedCodes = computed(() => batchCodes.value.map(displayCode).join('\n'))

function batchRate(batch: RedeemBatch) {
  return batch.total_count ? Math.round((batch.redeemed_count / batch.total_count) * 100) : 0
}

function batchStatusClass(batch: RedeemBatch) {
  if (batch.expires_at && batch.expires_at <= Math.floor(Date.now() / 1000)) return 'at_expired'
  if (batch.redeemed_count >= batch.total_count && batch.total_count > 0) return 'redeemed'
  return batch.status
}

function batchStatusLabel(batch: RedeemBatch) {
  if (batch.expires_at && batch.expires_at <= Math.floor(Date.now() / 1000)) return '已过期'
  if (batch.redeemed_count >= batch.total_count && batch.total_count > 0) return '已兑完'
  return statusLabel(batch.status)
}

async function selectBatch(batchId: string) {
  await loadCodes(batchId)
}

async function copyGeneratedCodes() {
  await copyText(generatedCodes.value, '已复制本次生成的完整兑换码')
}

function exportGeneratedCodes() {
  if (!generatedCodes.value.trim()) {
    toast.info('暂无可导出的完整兑换码')
    return
  }
  downloadText(`account-pool-generated-codes-${timestamp()}.txt`, generatedCodes.value)
  toast.success('已导出本次生成的完整兑换码')
}

async function copySelectedBatchCodes() {
  if (!selectedBatch.value) {
    toast.info('请先选择一个批次')
    return
  }
  await copyText(selectedCodes.value, '已复制该批次兑换码')
}

function exportSelectedBatchCodes() {
  if (!selectedBatch.value) {
    toast.info('请先选择一个批次')
    return
  }
  exportBatchSnapshot(selectedBatch.value)
}

async function copyBatchCodes(batch: RedeemBatch) {
  await loadCodes(batch.id)
  await copySelectedBatchCodes()
}

async function exportBatchCodes(batch: RedeemBatch) {
  await loadCodes(batch.id)
  exportBatchSnapshot(batch)
}

function exportBatchSnapshot(batch: RedeemBatch) {
  downloadText(
    `account-pool-redeem-codes-${safeFileName(batch.name)}-${timestamp()}.csv`,
    buildBatchCsv(batch),
    'text/csv;charset=utf-8',
  )
  toast.success('已导出批次兑换状态 CSV')
}

function buildBatchCsv(batch: RedeemBatch) {
  const exportedAt = new Date().toISOString()
  const rows = batchCodes.value.length
    ? batchCodes.value.map((code) => [
      exportedAt,
      batch.id,
      batch.name,
      batch.pool_id,
      batch.pool_name || poolLabel(batch.pool_id),
      batch.status,
      batch.total_count,
      batch.redeemed_count,
      batch.accounts_per_code,
      batch.after_sale_limit,
      formatCsvTime(batch.expires_at),
      code.id,
      displayCode(code),
      code.code ? '' : code.masked_code,
      code.status,
      formatBoundAccounts(code),
      formatCsvTime(code.redeemed_at),
      code.redemption_id || '',
      code.after_sale_count || 0,
      formatAfterSales(code),
      formatCsvTime(code.created_at),
      formatCsvTime(code.updated_at),
    ])
    : [[
      exportedAt,
      batch.id,
      batch.name,
      batch.pool_id,
      batch.pool_name || poolLabel(batch.pool_id),
      batch.status,
      batch.total_count,
      batch.redeemed_count,
      batch.accounts_per_code,
      batch.after_sale_limit,
      formatCsvTime(batch.expires_at),
      '',
      '',
      '',
      '',
      '',
      '',
      '',
      '',
      '',
      '',
      '',
    ]]
  const header = [
    'exported_at',
    'batch_id',
    'batch_name',
    'pool_id',
    'pool_name',
    'batch_status',
    'total_count',
    'redeemed_count',
    'accounts_per_code',
    'after_sale_limit',
    'batch_expires_at',
    'code_id',
    'code',
    'masked_code_fallback',
    'code_status',
    'bound_accounts',
    'code_redeemed_at',
    'redemption_id',
    'after_sale_count',
    'after_sales',
    'code_created_at',
    'code_updated_at',
  ]
  return `\uFEFF${[header, ...rows].map((row) => row.map(csvCell).join(',')).join('\n')}`
}

function displayCode(code: RedeemCode) {
  return code.code || code.masked_code
}

function boundAccountIds(code: RedeemCode) {
  return code.accounts.filter((account) => account.status !== 'deleted').map((account) => account.id)
}

function accountDisplayName(account: RedeemCode['accounts'][number]) {
  return account.email || account.name || account.account_id || account.id
}

function accountDetailText(account: RedeemCode['accounts'][number]) {
  const parts = [
    account.account_id && account.account_id !== account.id ? account.account_id : '',
    account.plan_type || '',
    account.last_probe_at ? `测活 ${formatTime(account.last_probe_at)}` : '',
  ].filter(Boolean)
  return parts.length ? parts.join(' / ') : account.id
}

function formatBoundAccounts(code: RedeemCode) {
  return code.accounts
    .map((account) => `${accountDisplayName(account)} (${account.status})`)
    .join('; ')
}

function formatAfterSaleAccounts(afterSale: RedeemAfterSale) {
  const oldAccounts = afterSale.old_accounts.map(accountDisplayName).join('、') || '-'
  const newAccounts = afterSale.new_accounts.map(accountDisplayName).join('、') || '-'
  return `${oldAccounts} -> ${newAccounts}`
}

function formatAfterSales(code: RedeemCode) {
  return code.after_sales
    .map((afterSale) => `${formatTime(afterSale.created_at)} ${formatAfterSaleAccounts(afterSale)}`)
    .join('; ')
}

async function probeCodeAccounts(code: RedeemCode) {
  const accountIds = boundAccountIds(code)
  if (!accountIds.length) {
    toast.info('这个兑换码暂无可测活账号')
    return
  }
  probingCodeId.value = code.id
  try {
    await api.probeAccounts(apiState.value, accountIds)
    if (selectedBatchId.value) {
      await loadCodes(selectedBatchId.value)
    }
    toast.success(`已测活 ${accountIds.length} 个绑定账号`)
  } catch (error) {
    toast.error(error instanceof Error ? error.message : String(error))
  } finally {
    probingCodeId.value = ''
  }
}

function csvCell(value: unknown) {
  const text = value == null ? '' : String(value)
  return `"${text.replace(/"/g, '""')}"`
}

function formatCsvTime(value?: number | null) {
  return value ? new Date(value * 1000).toISOString() : ''
}

async function copyText(value: string, successMessage: string) {
  if (!value.trim()) {
    toast.info('暂无可复制内容')
    return
  }
  try {
    fallbackCopy(value)
    toast.success(successMessage)
  } catch {
    try {
      await navigator.clipboard.writeText(value)
      toast.success(successMessage)
    } catch {
      manualCopyTitle.value = '手动复制兑换码'
      manualCopyText.value = value
      toast.info('浏览器限制自动复制，已显示可手动复制内容')
    }
  }
}

function selectManualCopyText(event: Event) {
  const target = event.target as HTMLTextAreaElement
  target.select()
}

function fallbackCopy(value: string) {
  const textarea = document.createElement('textarea')
  textarea.value = value
  textarea.setAttribute('readonly', 'true')
  textarea.style.position = 'fixed'
  textarea.style.opacity = '0'
  document.body.append(textarea)
  textarea.select()
  const copied = document.execCommand('copy')
  textarea.remove()
  if (!copied) throw new Error('copy failed')
}

function safeFileName(value: string) {
  return value.trim().replace(/[^a-zA-Z0-9._-]+/g, '-').replace(/^-+|-+$/g, '') || 'batch'
}
</script>
