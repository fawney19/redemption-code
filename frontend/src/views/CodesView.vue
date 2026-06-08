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
            <h2>兑换码批次</h2>
            <p>复制、导出、查看兑换状态</p>
          </div>
          <div class="toolbar compact-toolbar">
            <button class="button code-generator-toggle-button" :class="{ primary: !codeGeneratorOpen, active: codeGeneratorOpen }" :disabled="busy" @click="toggleCodeGenerator">
              <ChevronDown v-if="codeGeneratorOpen" :size="15" />
              <Plus v-else :size="15" />{{ codeGeneratorOpen ? '收起配置' : '生成兑换码' }}
            </button>
            <button class="button" :disabled="!selectedBatch" @click="copySelectedBatchCodes">
              <Copy :size="15" />复制
            </button>
            <button class="button" :disabled="!selectedBatch" @click="exportSelectedBatchCodes">
              <Download :size="15" />导出 CSV
            </button>
            <button class="button" :disabled="!selectedBatch" @click="exportSelectedBatchCodesTxt">
              <Download :size="15" />导出 TXT
            </button>
            <button class="button" :disabled="!selectedBatch" @click="selectedBatch && openEditBatch(selectedBatch)">
              <Settings :size="15" />编辑
            </button>
            <button class="button" @click="loadBatches">
              <RefreshCw :size="15" />刷新
            </button>
          </div>
        </div>
        <div class="panel-body grid">
          <Transition name="fade">
            <div v-if="codeGeneratorOpen" class="code-generator-panel">
              <div class="code-generator-card">
                <div class="code-generator-card-body">
                  <div class="code-generator-config">
                    <div class="code-generator-form">
                      <label class="field-label code-generator-field code-generator-field--pool">
                        <span>号池</span>
                        <select v-model="batchForm.pool_id" class="select">
                          <option v-for="pool in activePools" :key="pool.id" :value="pool.id">
                            {{ poolLabel(pool.id) }}
                          </option>
                        </select>
                      </label>
                      <label class="field-label code-generator-field code-generator-field--name">
                        <span>批次名称</span>
                        <input v-model="batchForm.name" class="input" placeholder="例如 6 月 Pro 批次" />
                      </label>
                      <label class="field-label code-generator-field code-generator-field--count">
                        <span>兑换码数量</span>
                        <input v-model.number="batchForm.total_count" class="input" type="number" min="1" max="10000" />
                      </label>
                      <div class="code-generator-form-actions">
                        <button class="button primary" :disabled="busy" @click="handleCreateBatch">
                          <Plus :size="15" />生成
                        </button>
                      </div>
                      <label class="field-label code-generator-field code-generator-field--compact">
                        <span>每码账号数</span>
                        <input v-model.number="batchForm.accounts_per_code" class="input" type="number" min="1" max="100" />
                      </label>
                      <label class="field-label code-generator-field code-generator-field--compact">
                        <span>每码售后次数</span>
                        <input v-model.number="batchForm.after_sale_limit" class="input" type="number" min="0" max="10" />
                      </label>
                      <div class="field-label code-generator-field code-generator-field--expiry">
                        <span>过期时间</span>
                        <div class="expiry-input-row">
                          <input v-model="batchForm.expires_at_text" class="input" type="datetime-local" />
                          <select v-model="expiryQuickDays" class="select expiry-quick-select" aria-label="快捷设置过期时间" @change="applyExpiryQuickSelect">
                            <option value="">不限制</option>
                            <option value="1">1天</option>
                            <option value="3">3天</option>
                            <option value="7">7天</option>
                            <option value="30">30天</option>
                          </select>
                        </div>
                      </div>
                    </div>
                  </div>
                  <div class="generated-codes-wrap">
                    <div class="terminal-bar generated-codes-terminal-bar">
                      <div class="terminal-dots"><span></span><span></span><span></span></div>
                      <div class="generated-codes-terminal-actions">
                        <button class="button ghost tiny" :disabled="!generatedCodes.trim()" @click="copyGeneratedCodes">
                          <Copy :size="15" />复制
                        </button>
                        <button class="button ghost tiny" :disabled="!generatedCodes.trim()" @click="exportGeneratedCodes">
                          <Download :size="15" />导出 TXT
                        </button>
                      </div>
                    </div>
                    <pre class="result mono dark-result generated-codes-result" :class="{ 'generated-codes-result--empty': !generatedCodes.trim() }">{{ generatedCodesPreview || '暂无生成结果' }}</pre>
                  </div>
                </div>
              </div>
            </div>
          </Transition>

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
                  <th>
                    <div class="table-filter-heading">
                      <div class="table-filter-control">
                        <button
                          class="table-filter-button"
                          :class="{ active: batchPoolFilters.length }"
                          type="button"
                          title="筛选号池"
                          aria-label="筛选号池"
                          :aria-expanded="batchHeaderFilterMenu === 'pool'"
                          @click="toggleBatchHeaderFilterMenu('pool')"
                        >
                          <ListFilter :size="15" />
                        </button>
                        <div v-if="batchHeaderFilterMenu === 'pool'" class="table-filter-menu">
                          <div class="filter-menu-title">号池</div>
                          <label v-for="option in batchPoolFilterOptions" :key="option.value" class="filter-option pool-filter-option">
                            <input type="checkbox" :checked="batchPoolFilters.includes(option.value)" @change="toggleBatchPoolFilter(option.value)" />
                            <span class="filter-option-content">
                              <strong>{{ option.label }}</strong>
                              <small>{{ option.value }}</small>
                            </span>
                          </label>
                          <button v-if="batchPoolFilters.length" class="filter-clear-button" type="button" @click="clearBatchPoolFilter">清除号池筛选</button>
                        </div>
                      </div>
                      <span>号池</span>
                    </div>
                  </th>
                  <th>兑换进度</th>
                  <th>每码</th>
                  <th>售后</th>
                  <th>
                    <div class="table-filter-heading">
                      <div class="table-filter-control">
                        <button
                          class="table-filter-button"
                          :class="{ active: batchStatusFilters.length }"
                          type="button"
                          title="筛选状态"
                          aria-label="筛选状态"
                          :aria-expanded="batchHeaderFilterMenu === 'status'"
                          @click="toggleBatchHeaderFilterMenu('status')"
                        >
                          <ListFilter :size="15" />
                        </button>
                        <div v-if="batchHeaderFilterMenu === 'status'" class="table-filter-menu state-filter-menu">
                          <div class="filter-menu-title">状态</div>
                          <label v-for="option in batchStatusFilterOptions" :key="option.value" class="filter-option">
                            <input type="checkbox" :checked="batchStatusFilters.includes(option.value)" @change="toggleBatchStatusFilter(option.value)" />
                            <span>{{ option.label }}</span>
                          </label>
                          <button v-if="batchStatusFilters.length" class="filter-clear-button" type="button" @click="clearBatchStatusFilter">清除状态筛选</button>
                        </div>
                      </div>
                      <span>状态</span>
                    </div>
                  </th>
                  <th>操作</th>
                </tr>
              </thead>
              <tbody>
                <template v-for="batch in filteredBatches" :key="batch.id">
                  <tr :class="{ selected: batch.id === selectedBatchId }">
                    <td>
                      <div class="batch-title-cell">
                        <button
                          class="button ghost tiny icon-only batch-expand-button"
                          :disabled="busy"
                          :title="batch.id === selectedBatchId ? '收起兑换码明细' : '展开兑换码明细'"
                          :aria-label="batch.id === selectedBatchId ? '收起兑换码明细' : '展开兑换码明细'"
                          :aria-expanded="batch.id === selectedBatchId"
                          @click="toggleBatchExpansion(batch)"
                        >
                          <ChevronDown v-if="batch.id === selectedBatchId" :size="14" />
                          <ChevronRight v-else :size="14" />
                        </button>
                        <div>
                          <strong>{{ batch.name }}</strong>
                          <div class="muted mono">{{ batch.id }}</div>
                        </div>
                      </div>
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
                        <button class="button ghost tiny" @click="copyBatchCodes(batch)">复制</button>
                        <button class="button ghost tiny" @click="exportBatchCodes(batch)">导出 CSV</button>
                        <button class="button ghost tiny" @click="exportBatchCodesTxt(batch)">导出 TXT</button>
                        <button class="button tiny" :class="batch.status === 'disabled' ? 'ghost' : 'danger'" :disabled="busy" @click="toggleBatchStatus(batch)">
                          <RefreshCw v-if="batch.status === 'disabled'" :size="14" />
                          <Settings v-else :size="14" />{{ batch.status === 'disabled' ? '启用' : '禁用' }}
                        </button>
                        <button class="button ghost tiny" :disabled="busy" @click="openEditBatch(batch)">
                          <Settings :size="14" />编辑
                        </button>
                        <button class="button danger tiny" :disabled="busy" @click="deleteBatch(batch)">
                          <Trash2 :size="14" />删除
                        </button>
                      </div>
                    </td>
                  </tr>
                  <tr v-if="batch.id === selectedBatchId" class="batch-expanded-row">
                    <td colspan="7">
                      <div class="batch-expanded-panel">
                        <div class="batch-expanded-header">
                          <div>
                            <strong>兑换码明细</strong>
                            <span>{{ batch.name }} / {{ batch.id }}</span>
                          </div>
                          <div class="detail-stats">
                            <span>{{ selectedBatchStats.redeemedCodes }} 已兑</span>
                            <span>{{ selectedBatchStats.activeCodes }} 可兑</span>
                            <span>{{ selectedBatchStats.afterSaleCount }} 售后</span>
                            <span>{{ selectedBatchStats.redemptionRate }}%</span>
                          </div>
                          <div class="batch-detail-pagination">
                            <div class="pagination-summary">
                              <strong>{{ detailPageStart }}-{{ detailPageEnd }}</strong>
                              <span>/ {{ detailTotalCodes }} 个兑换码</span>
                            </div>
                            <div class="pagination-controls">
                              <label>
                                <span>每页</span>
                                <select v-model.number="detailPageSize" class="select page-size-select" @change="changeDetailPageSize">
                                  <option :value="10">10</option>
                                  <option :value="25">25</option>
                                  <option :value="50">50</option>
                                </select>
                              </label>
                              <span class="pagination-page">第 {{ detailCurrentPage }} / {{ detailTotalPages }} 页</span>
                              <button class="button ghost tiny" :disabled="busy || !canPrevDetailPage" @click="previousDetailPage">上一页</button>
                              <button class="button ghost tiny" :disabled="busy || !canNextDetailPage" @click="nextDetailPage">下一页</button>
                            </div>
                          </div>
                        </div>
                        <div class="table-wrap detail-table-wrap batch-code-table-wrap">
                          <table class="table detail-table">
                            <thead>
                              <tr>
                                <th>兑换码</th>
                                <th>
                                  <div class="table-filter-heading">
                                    <div class="table-filter-control">
                                      <button
                                        class="table-filter-button"
                                        :class="{ active: codeStatusFilters.length }"
                                        type="button"
                                        title="筛选兑换码状态"
                                        aria-label="筛选兑换码状态"
                                        :aria-expanded="batchHeaderFilterMenu === 'code-status'"
                                        @click="toggleBatchHeaderFilterMenu('code-status')"
                                      >
                                        <ListFilter :size="15" />
                                      </button>
                                      <div v-if="batchHeaderFilterMenu === 'code-status'" class="table-filter-menu state-filter-menu">
                                        <div class="filter-menu-title">兑换码状态</div>
                                        <label v-for="option in codeStatusFilterOptions" :key="option.value" class="filter-option">
                                          <input type="checkbox" :checked="codeStatusFilters.includes(option.value)" @change="toggleCodeStatusFilter(option.value)" />
                                          <span>{{ option.label }}</span>
                                        </label>
                                        <button v-if="codeStatusFilters.length" class="filter-clear-button" type="button" @click="clearCodeStatusFilter">清除状态筛选</button>
                                      </div>
                                    </div>
                                    <span>状态</span>
                                  </div>
                                </th>
                                <th>绑定账号</th>
                                <th>兑换时间</th>
                                <th>兑换记录</th>
                                <th>售后</th>
                                <th>操作</th>
                              </tr>
                            </thead>
                            <tbody>
                              <tr v-for="code in pagedBatchCodes" :key="code.id">
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
                                  <div class="row-actions">
                                    <button
                                      v-if="canToggleCodeStatus(code)"
                                      class="button tiny"
                                      :class="code.status === 'disabled' ? 'ghost' : 'danger'"
                                      :disabled="busy"
                                      @click="toggleCodeStatus(code)"
                                    >
                                      <RefreshCw v-if="code.status === 'disabled'" :size="14" />
                                      <Settings v-else :size="14" />{{ code.status === 'disabled' ? '启用' : '禁用' }}
                                    </button>
                                    <button
                                      class="button ghost tiny"
                                      :disabled="busy || probingCodeId === code.id || !boundAccountIds(code).length"
                                      @click="probeCodeAccounts(code)"
                                    >
                                      <Activity :size="14" />测活
                                    </button>
                                  </div>
                                </td>
                              </tr>
                              <tr v-if="!filteredBatchCodes.length">
                                <td colspan="7" class="empty-row">
                                  <Ticket :size="20" />
                                  <span>{{ batchCodes.length ? '暂无匹配兑换码' : '暂无兑换码明细' }}</span>
                                </td>
                              </tr>
                            </tbody>
                          </table>
                        </div>
                      </div>
                    </td>
                  </tr>
                </template>
                <tr v-if="!filteredBatches.length">
                  <td colspan="7" class="empty-row">
                    <Ticket :size="20" />
                    <span>{{ batches.length ? '暂无匹配批次' : '暂无批次' }}</span>
                  </td>
                </tr>
              </tbody>
            </table>
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

    <Transition name="fade">
      <div v-if="editingBatch" class="modal-backdrop" @click.self="closeEditBatch">
        <section class="modal-panel batch-edit-modal" aria-label="编辑兑换码批次配置">
          <header class="modal-header">
            <div>
              <h2>编辑批次配置</h2>
              <span class="mono">{{ editingBatch.id }}</span>
            </div>
            <button class="button ghost tiny icon-only" type="button" :disabled="busy" aria-label="关闭" @click="closeEditBatch">
              <span aria-hidden="true">X</span>
            </button>
          </header>
          <div class="batch-edit-form">
            <label class="field-label full">
              <span>批次名称</span>
              <input v-model="editBatchForm.name" class="input" />
            </label>
            <label class="field-label">
              <span>状态</span>
              <select v-model="editBatchForm.status" class="select">
                <option value="active">可兑换</option>
                <option value="disabled">已停用</option>
              </select>
            </label>
            <label class="field-label">
              <span>每码账号数</span>
              <input
                v-model.number="editBatchForm.accounts_per_code"
                class="input"
                type="number"
                min="1"
                max="100"
                :disabled="editingBatch.redeemed_count > 0"
              />
            </label>
            <label class="field-label">
              <span>每码售后次数</span>
              <input v-model.number="editBatchForm.after_sale_limit" class="input" type="number" min="0" max="10" />
            </label>
            <div class="field-label batch-edit-expiry">
              <span>过期时间</span>
              <div class="expiry-input-row">
                <input v-model="editBatchForm.expires_at_text" class="input" type="datetime-local" />
                <select v-model="editExpiryQuickDays" class="select expiry-quick-select" aria-label="快捷设置编辑过期时间" @change="applyEditExpiryQuickSelect">
                  <option value="">不限制</option>
                  <option value="1">1天</option>
                  <option value="3">3天</option>
                  <option value="7">7天</option>
                  <option value="30">30天</option>
                </select>
              </div>
            </div>
          </div>
          <footer class="modal-actions">
            <button class="button ghost" type="button" :disabled="busy" @click="closeEditBatch">取消</button>
            <button class="button primary" type="button" :disabled="busy" @click="submitEditBatch">
              <Save :size="15" />保存
            </button>
          </footer>
        </section>
      </div>
    </Transition>
  </section>
</template>

<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { Activity, ChevronDown, ChevronRight, Copy, Download, ListFilter, Plus, RefreshCw, Save, Settings, Ticket, Trash2 } from 'lucide-vue-next'
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
const codeGeneratorOpen = ref(true)
const expiryQuickDays = ref('')
const detailPageSize = ref(10)
const detailPage = ref(1)
const batchHeaderFilterMenu = ref<'pool' | 'status' | 'code-status' | ''>('')
const batchPoolFilters = ref<string[]>([])
const batchStatusFilters = ref<string[]>([])
const codeStatusFilters = ref<string[]>([])
const editingBatch = ref<RedeemBatch | null>(null)
const editExpiryQuickDays = ref('')
const editBatchForm = reactive({
  name: '',
  status: 'active',
  accounts_per_code: 1,
  after_sale_limit: 1,
  expires_at_text: '',
})

const generatedCodePreviewLimit = 200
const generatedCodeLines = computed(() => generatedCodes.value.split(/\r?\n/).filter(Boolean))
const generatedCodeCount = computed(() => generatedCodeLines.value.length)
const generatedCodesPreview = computed(() => {
  const lines = generatedCodeLines.value
  if (lines.length <= generatedCodePreviewLimit) return generatedCodes.value
  return [
    ...lines.slice(0, generatedCodePreviewLimit),
    `... 已隐藏 ${lines.length - generatedCodePreviewLimit} 个，复制/导出会包含全部`,
  ].join('\n')
})
const selectedCodes = computed(() => batchCodes.value.map(displayCode).join('\n'))
const batchPoolFilterOptions = computed(() => {
  const options = new Map<string, string>()
  for (const batch of batches.value) {
    options.set(batch.pool_id, batch.pool_name || poolLabel(batch.pool_id))
  }
  return [...options].map(([value, label]) => ({ value, label }))
})
const batchStatusFilterOptions = computed(() => {
  const options = new Map<string, string>()
  for (const batch of batches.value) {
    options.set(batchStatusFilterValue(batch), batchStatusLabel(batch))
  }
  return [...options].map(([value, label]) => ({ value, label }))
})
const codeStatusFilterOptions = computed(() => {
  const options = new Map<string, string>()
  for (const code of batchCodes.value) {
    options.set(code.status, statusLabel(code.status))
  }
  return [...options].map(([value, label]) => ({ value, label }))
})
const filteredBatches = computed(() => batches.value.filter((batch) => {
  if (batchPoolFilters.value.length && !batchPoolFilters.value.includes(batch.pool_id)) return false
  if (batchStatusFilters.value.length && !batchStatusFilters.value.includes(batchStatusFilterValue(batch))) return false
  return true
}))
const filteredBatchCodes = computed(() => batchCodes.value.filter((code) => {
  if (codeStatusFilters.value.length && !codeStatusFilters.value.includes(code.status)) return false
  return true
}))
const detailTotalCodes = computed(() => filteredBatchCodes.value.length)
const detailTotalPages = computed(() => Math.max(1, Math.ceil(detailTotalCodes.value / detailPageSize.value)))
const detailCurrentPage = computed(() => Math.min(detailPage.value, detailTotalPages.value))
const detailPageStartIndex = computed(() => (detailCurrentPage.value - 1) * detailPageSize.value)
const detailPageStart = computed(() => detailTotalCodes.value ? detailPageStartIndex.value + 1 : 0)
const detailPageEnd = computed(() => Math.min(detailPageStartIndex.value + detailPageSize.value, detailTotalCodes.value))
const pagedBatchCodes = computed(() => filteredBatchCodes.value.slice(detailPageStartIndex.value, detailPageEnd.value))
const canPrevDetailPage = computed(() => detailCurrentPage.value > 1)
const canNextDetailPage = computed(() => detailCurrentPage.value < detailTotalPages.value)

onMounted(() => {
  document.addEventListener('click', closeBatchFilterMenuOnOutsideClick)
})

onBeforeUnmount(() => {
  document.removeEventListener('click', closeBatchFilterMenuOnOutsideClick)
})

function batchRate(batch: RedeemBatch) {
  return batch.total_count ? Math.round((batch.redeemed_count / batch.total_count) * 100) : 0
}

function batchStatusClass(batch: RedeemBatch) {
  if (batch.expires_at && batch.expires_at <= Math.floor(Date.now() / 1000)) return 'at_expired'
  if (batch.redeemed_count >= batch.total_count && batch.total_count > 0) return 'redeemed'
  return batch.status
}

function batchStatusFilterValue(batch: RedeemBatch) {
  return batchStatusClass(batch)
}

function batchStatusLabel(batch: RedeemBatch) {
  if (batch.expires_at && batch.expires_at <= Math.floor(Date.now() / 1000)) return '已过期'
  if (batch.redeemed_count >= batch.total_count && batch.total_count > 0) return '已兑完'
  return statusLabel(batch.status)
}

function toggleBatchHeaderFilterMenu(menu: 'pool' | 'status' | 'code-status') {
  batchHeaderFilterMenu.value = batchHeaderFilterMenu.value === menu ? '' : menu
}

function closeBatchFilterMenuOnOutsideClick(event: MouseEvent) {
  if (!batchHeaderFilterMenu.value) return
  const target = event.target
  if (target instanceof Element && target.closest('.table-filter-control')) return
  batchHeaderFilterMenu.value = ''
}

function toggleArrayValue(values: string[], value: string) {
  const index = values.indexOf(value)
  if (index >= 0) values.splice(index, 1)
  else values.push(value)
}

function toggleBatchPoolFilter(poolId: string) {
  toggleArrayValue(batchPoolFilters.value, poolId)
  collapseHiddenBatch()
}

function toggleBatchStatusFilter(status: string) {
  toggleArrayValue(batchStatusFilters.value, status)
  collapseHiddenBatch()
}

function clearBatchPoolFilter() {
  batchPoolFilters.value = []
  collapseHiddenBatch()
}

function clearBatchStatusFilter() {
  batchStatusFilters.value = []
  collapseHiddenBatch()
}

function toggleCodeStatusFilter(status: string) {
  toggleArrayValue(codeStatusFilters.value, status)
  detailPage.value = 1
}

function clearCodeStatusFilter() {
  codeStatusFilters.value = []
  detailPage.value = 1
}

function resetCodeDetailState() {
  codeStatusFilters.value = []
  detailPage.value = 1
  if (batchHeaderFilterMenu.value === 'code-status') batchHeaderFilterMenu.value = ''
}

function collapseHiddenBatch() {
  if (!selectedBatchId.value) return
  if (filteredBatches.value.some((batch) => batch.id === selectedBatchId.value)) return
  selectedBatchId.value = ''
  batchCodes.value = []
  resetCodeDetailState()
}

async function toggleBatchExpansion(batch: RedeemBatch) {
  if (selectedBatchId.value === batch.id) {
    selectedBatchId.value = ''
    batchCodes.value = []
    resetCodeDetailState()
    return
  }
  resetCodeDetailState()
  await loadCodes(batch.id)
}

function previousDetailPage() {
  if (!canPrevDetailPage.value) return
  detailPage.value -= 1
}

function nextDetailPage() {
  if (!canNextDetailPage.value) return
  detailPage.value += 1
}

function changeDetailPageSize() {
  detailPage.value = 1
}

function toggleCodeGenerator() {
  codeGeneratorOpen.value = !codeGeneratorOpen.value
}

function setBatchExpiryDays(days: number) {
  const expiresAt = new Date()
  expiresAt.setDate(expiresAt.getDate() + days)
  expiresAt.setSeconds(0, 0)
  batchForm.expires_at_text = formatDateTimeLocal(expiresAt)
}

function applyExpiryQuickSelect() {
  const days = Number(expiryQuickDays.value)
  if (!days) {
    batchForm.expires_at_text = ''
    return
  }
  setBatchExpiryDays(days)
}

function openEditBatch(batch: RedeemBatch) {
  editingBatch.value = batch
  editBatchForm.name = batch.name
  editBatchForm.status = batch.status === 'disabled' ? 'disabled' : 'active'
  editBatchForm.accounts_per_code = Number(batch.accounts_per_code || 1)
  editBatchForm.after_sale_limit = Number(batch.after_sale_limit ?? 1)
  editBatchForm.expires_at_text = batch.expires_at
    ? formatDateTimeLocal(new Date(batch.expires_at * 1000))
    : ''
  editExpiryQuickDays.value = ''
}

function closeEditBatch() {
  if (busy.value) return
  editingBatch.value = null
  editExpiryQuickDays.value = ''
}

function setEditBatchExpiryDays(days: number) {
  const expiresAt = new Date()
  expiresAt.setDate(expiresAt.getDate() + days)
  expiresAt.setSeconds(0, 0)
  editBatchForm.expires_at_text = formatDateTimeLocal(expiresAt)
}

function applyEditExpiryQuickSelect() {
  const days = Number(editExpiryQuickDays.value)
  if (!days) {
    editBatchForm.expires_at_text = ''
    return
  }
  setEditBatchExpiryDays(days)
}

function formatDateTimeLocal(value: Date) {
  const pad = (part: number) => String(part).padStart(2, '0')
  return [
    value.getFullYear(),
    '-',
    pad(value.getMonth() + 1),
    '-',
    pad(value.getDate()),
    'T',
    pad(value.getHours()),
    ':',
    pad(value.getMinutes()),
  ].join('')
}

function parseEditBatchExpiresAt() {
  const raw = editBatchForm.expires_at_text.trim()
  if (!raw) return null
  const value = Math.floor(new Date(raw).getTime() / 1000)
  if (!Number.isFinite(value)) throw new Error('过期时间无效')
  return value
}

async function submitEditBatch() {
  const batch = editingBatch.value
  if (!batch || busy.value) return
  if (!editBatchForm.name.trim()) {
    toast.info('批次名称不能为空')
    return
  }
  busy.value = true
  try {
    const result = await api.updateBatch(apiState.value, batch.id, {
      name: editBatchForm.name.trim(),
      status: editBatchForm.status,
      accounts_per_code: Number(editBatchForm.accounts_per_code || 1),
      after_sale_limit: Number(editBatchForm.after_sale_limit ?? 1),
      expires_at: parseEditBatchExpiresAt(),
    })
    await loadBatches()
    editingBatch.value = null
    editExpiryQuickDays.value = ''
    toast.success(`已更新批次配置：${result.batch.name}`)
  } catch (error) {
    toast.error(error instanceof Error ? error.message : String(error))
  } finally {
    busy.value = false
  }
}

async function toggleBatchStatus(batch: RedeemBatch) {
  if (busy.value) return
  const nextStatus = batch.status === 'disabled' ? 'active' : 'disabled'
  busy.value = true
  try {
    const result = await api.updateBatch(apiState.value, batch.id, {
      name: batch.name,
      status: nextStatus,
      accounts_per_code: Number(batch.accounts_per_code || 1),
      after_sale_limit: Number(batch.after_sale_limit ?? 1),
      expires_at: batch.expires_at ?? null,
    })
    replaceBatch(result.batch)
    collapseHiddenBatch()
    toast.success(`已${nextStatus === 'active' ? '启用' : '禁用'}批次：${result.batch.name}`)
  } catch (error) {
    toast.error(error instanceof Error ? error.message : String(error))
  } finally {
    busy.value = false
  }
}

function canToggleCodeStatus(code: RedeemCode) {
  return (code.status === 'active' || code.status === 'disabled') && !code.redemption_id && !code.redeemed_at
}

async function toggleCodeStatus(code: RedeemCode) {
  if (busy.value || !canToggleCodeStatus(code)) return
  const nextStatus = code.status === 'disabled' ? 'active' : 'disabled'
  busy.value = true
  try {
    const result = await api.updateCode(apiState.value, code.batch_id, code.id, { status: nextStatus })
    replaceBatchCode(result.code)
    toast.success(`已${nextStatus === 'active' ? '启用' : '禁用'}兑换码`)
  } catch (error) {
    toast.error(error instanceof Error ? error.message : String(error))
  } finally {
    busy.value = false
  }
}

function replaceBatch(updated: RedeemBatch) {
  const index = batches.value.findIndex((batch) => batch.id === updated.id)
  if (index >= 0) batches.value.splice(index, 1, updated)
}

function replaceBatchCode(updated: RedeemCode) {
  const index = batchCodes.value.findIndex((code) => code.id === updated.id)
  if (index >= 0) batchCodes.value.splice(index, 1, updated)
}

async function handleCreateBatch() {
  await createBatch()
  codeGeneratorOpen.value = true
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
  await ensureSelectedBatchCodesLoaded()
  await copyText(selectedCodes.value, '已复制该批次兑换码')
}

async function exportSelectedBatchCodes() {
  if (!selectedBatch.value) {
    toast.info('请先选择一个批次')
    return
  }
  await ensureSelectedBatchCodesLoaded()
  exportBatchSnapshot(selectedBatch.value)
}

async function exportSelectedBatchCodesTxt() {
  if (!selectedBatch.value) {
    toast.info('请先选择一个批次')
    return
  }
  await ensureSelectedBatchCodesLoaded()
  exportBatchPlainText(selectedBatch.value)
}

async function copyBatchCodes(batch: RedeemBatch) {
  const codes = await fetchBatchCodesSnapshot(batch.id)
  await copyText(codes.map(displayCode).join('\n'), '已复制该批次兑换码')
}

async function exportBatchCodes(batch: RedeemBatch) {
  const codes = await fetchBatchCodesSnapshot(batch.id)
  exportBatchSnapshot(batch, codes)
}

async function exportBatchCodesTxt(batch: RedeemBatch) {
  const codes = await fetchBatchCodesSnapshot(batch.id)
  exportBatchPlainText(batch, codes)
}

async function ensureSelectedBatchCodesLoaded() {
  if (selectedBatch.value && !batchCodes.value.length) {
    await loadCodes(selectedBatch.value.id)
  }
}

async function fetchBatchCodesSnapshot(batchId: string) {
  if (selectedBatchId.value === batchId && batchCodes.value.length) return batchCodes.value
  const wasBusy = busy.value
  if (!wasBusy) busy.value = true
  try {
    const result = await api.listCodes(apiState.value, batchId)
    return result.items
  } finally {
    if (!wasBusy) busy.value = false
  }
}

function exportBatchSnapshot(batch: RedeemBatch, codes = batchCodes.value) {
  downloadText(
    `account-pool-redeem-codes-${safeFileName(batch.name)}-${timestamp()}.csv`,
    buildBatchCsv(batch, codes),
    'text/csv;charset=utf-8',
  )
  toast.success('已导出批次兑换状态 CSV')
}

function exportBatchPlainText(batch: RedeemBatch, batchCodeItems = batchCodes.value) {
  const codes = batchCodeItems
    .map((code) => code.code?.trim())
    .filter((code): code is string => Boolean(code))
  const skipped = batchCodeItems.length - codes.length
  if (!codes.length) {
    toast.info('该批次没有可导出的未脱敏兑换码')
    return
  }
  downloadText(
    `account-pool-redeem-codes-${safeFileName(batch.name)}-${timestamp()}.txt`,
    codes.join('\n'),
  )
  toast.success(skipped
    ? `已导出 ${codes.length} 个未脱敏兑换码，跳过 ${skipped} 个历史脱敏码`
    : `已导出 ${codes.length} 个未脱敏兑换码`)
}

function buildBatchCsv(batch: RedeemBatch, codes = batchCodes.value) {
  const exportedAt = new Date().toISOString()
  const rows = codes.length
    ? codes.map((code) => [
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
