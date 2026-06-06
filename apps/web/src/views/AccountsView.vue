<template>
  <section class="grid">
    <div class="stats">
      <div class="stat dark-stat">
        <span>总账号</span>
        <strong>{{ accountStats.total }}</strong>
      </div>
      <div class="stat">
        <span>总可分配</span>
        <strong>{{ availableCount }}</strong>
      </div>
      <div class="stat">
        <span>总已兑换</span>
        <strong>{{ redeemedCount }}</strong>
      </div>
      <div class="stat stat-attention">
        <span>总需处理</span>
        <strong>{{ attentionCount }}</strong>
      </div>
    </div>

    <div class="admin-grid">
      <div class="panel accounts-panel">
        <div class="panel-header">
          <div>
            <h2>账号列表</h2>
            <p>默认隐藏已兑换账号的自动刷新队列</p>
          </div>
          <div class="toolbar">
            <input v-model="filters.search" class="input search-input" placeholder="搜索邮箱 / Account ID" @keyup.enter="searchAccounts" />
            <select v-model="filters.status" class="select status-select" @change="searchAccounts">
              <option value="">全部状态</option>
              <option value="available">可用</option>
              <option value="at_expired">AT 过期</option>
              <option value="refresh_failed">刷新失败</option>
              <option value="quota_exhausted">额度耗尽</option>
              <option value="auth_invalid">账号失效</option>
              <option value="forbidden">网络受限</option>
            </select>
            <select v-model="filters.redeemed" class="select status-select" @change="searchAccounts">
              <option value="">全部兑换</option>
              <option value="false">未兑换</option>
              <option value="true">已兑换</option>
            </select>
            <button class="button" @click="searchAccounts"><Search :size="15" />查询</button>
          </div>
        </div>
        <div class="panel-body">
          <div class="toolbar section-toolbar">
            <button class="button" :disabled="busy" @click="probeSelected"><Activity :size="15" />测活</button>
            <button class="button" :disabled="busy" @click="refreshSelected"><RotateCcw :size="15" />刷新 AT</button>
            <button class="button danger" :disabled="busy || !selectedIds.length" @click="deleteSelectedAccounts">
              <Trash2 :size="15" />删除
            </button>
            <select v-model="adminExportFormat" class="select format-select">
              <option value="cpa">CPA</option>
              <option value="sub2api">Sub2API</option>
            </select>
            <button class="button primary" :disabled="busy" @click="exportSelected"><Download :size="15" />导出</button>
          </div>
          <div class="table-wrap">
            <table class="table">
              <thead>
                <tr>
                  <th><input type="checkbox" :checked="allSelected" @change="toggleAll" /></th>
                  <th>账号</th>
                  <th>号池</th>
                  <th>状态</th>
                  <th>绑定兑换码</th>
                  <th>兑换时间</th>
                  <th>Token</th>
                  <th>额度</th>
                  <th>过期</th>
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
                  <td><span class="badge" :class="statusBadgeClass(account.status)">{{ statusLabel(account.status) }}</span></td>
                  <td>
                    <span class="badge" :class="account.redeemed_at || account.redeem_code_id ? 'redeemed' : 'available'">
                      {{ account.redeemed_at || account.redeem_code_id ? '已兑换' : '未兑换' }}
                    </span>
                    <div class="muted mono">{{ account.redeem_code_masked || '-' }}</div>
                  </td>
                  <td>{{ formatTime(account.redeemed_at) }}</td>
                  <td>
                    <div class="token-stack mono">
                      <span><b>AT</b>{{ account.access_token_preview || '-' }}</span>
                      <span><b>RT</b>{{ account.refresh_token_preview || '-' }}</span>
                    </div>
                  </td>
                  <td>
                    <div class="quota-stack">
                      <div class="quota-row" :class="quotaUsageClass(account, 'five_hour')">
                        <span class="quota-label">5h</span>
                        <span class="quota-value">{{ quotaPercentText(account, 'five_hour') }}</span>
                        <span class="quota-track">
                          <span class="quota-fill" :style="{ width: quotaPercentBarWidth(account, 'five_hour') }"></span>
                        </span>
                      </div>
                      <div class="quota-row" :class="quotaUsageClass(account, 'weekly')">
                        <span class="quota-label">周</span>
                        <span class="quota-value">{{ quotaPercentText(account, 'weekly') }}</span>
                        <span class="quota-track">
                          <span class="quota-fill" :style="{ width: quotaPercentBarWidth(account, 'weekly') }"></span>
                        </span>
                      </div>
                    </div>
                  </td>
                  <td>{{ formatTime(account.expires_at) }}</td>
                  <td>{{ formatTime(account.last_probe_at) }}</td>
                  <td>
                    <button class="button ghost tiny" :disabled="busy" @click="probeAccount(account.id)">
                      <Activity :size="14" />测活
                    </button>
                    <button
                      class="button ghost danger tiny"
                      :disabled="busy || !canDeleteAccount(account)"
                      :title="canDeleteAccount(account) ? '删除账号' : '已兑换账号需测活为失效状态后才能删除'"
                      @click="deleteAccount(account.id)"
                    >
                      <Trash2 :size="14" />删除
                    </button>
                  </td>
                </tr>
                <tr v-if="!accounts.length">
                  <td colspan="11" class="empty-row">
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

      <div class="admin-side-stack">
        <div class="panel auto-probe-panel">
          <div class="panel-header">
            <div>
              <h2>自动测活</h2>
              <p>后台定时检测未兑换账号</p>
            </div>
            <span class="badge" :class="autoProbeSettings?.enabled ? 'available' : 'disabled'">
              {{ autoProbeSettings?.enabled ? '运行中' : '已关闭' }}
            </span>
          </div>
          <div class="panel-body grid">
            <label class="setting-toggle">
              <input v-model="autoProbeForm.enabled" type="checkbox" />
              <span>
                <strong>启用自动测活</strong>
                <small>仅处理未兑换账号</small>
              </span>
            </label>

            <div class="settings-grid">
              <label class="field-label">
                <span>间隔（分钟）</span>
                <input v-model.number="autoProbeIntervalMinutes" class="input" type="number" min="1" max="1440" />
              </label>
              <label class="field-label">
                <span>单次账号数</span>
                <input v-model.number="autoProbeForm.max_accounts_per_run" class="input" type="number" min="1" max="5000" />
              </label>
              <label class="field-label">
                <span>并发</span>
                <input v-model.number="autoProbeForm.concurrency" class="input" type="number" min="1" max="32" />
              </label>
            </div>

            <div class="settings-grid probe-mode-grid">
              <label class="field-label">
                <span>测活模式</span>
                <select v-model="autoProbeForm.probe_mode" class="select">
                  <option value="hybrid">Hybrid：CPA 优先</option>
                  <option value="direct">仅本服务直连</option>
                  <option value="cpa">仅 CPA 代打</option>
                </select>
              </label>
              <label class="setting-toggle compact-toggle">
                <input v-model="autoProbeForm.deep_check_enabled" type="checkbox" />
                <span>
                  <strong>深度诊断</strong>
                  <small>失败时检测 RT、session 与 accounts/check</small>
                </span>
              </label>
            </div>

            <div v-if="autoProbeForm.probe_mode !== 'direct'" class="proxy-settings">
              <div class="settings-grid">
                <label class="field-label full">
                  <span>CPA Base URL</span>
                  <input
                    v-model="autoProbeForm.cpa_base_url"
                    class="input"
                    placeholder="http://localhost:8317"
                  />
                </label>
                <label class="field-label full">
                  <span>CPA 管理密钥</span>
                  <input
                    v-model="autoProbeForm.cpa_management_key"
                    class="input"
                    type="password"
                    :placeholder="autoProbeForm.cpa_management_key_set ? '已保存，留空不修改' : '粘贴 X-Management-Key'"
                  />
                </label>
              </div>
              <div class="toolbar proxy-test-actions">
                <button class="button" :disabled="cpaTestDisabled" @click="testCpaConnection">
                  <Globe :size="15" />测试 CPA
                </button>
                <button class="button" :disabled="cpaScanDisabled" @click="scanCpa401">
                  <Activity :size="15" />CPA scan-401
                </button>
                <span class="form-note">{{ autoProbeForm.cpa_management_key_set ? 'CPA 密钥已保存' : 'CPA 密钥未保存' }}</span>
              </div>
              <div v-if="cpaTestResult" class="proxy-test-result">
                <div>
                  <span>CPA</span>
                  <strong class="mono">{{ cpaTestResult.base_url }}</strong>
                </div>
                <div>
                  <span>Auth 文件</span>
                  <strong>{{ cpaTestResult.auth_file_count }} 个</strong>
                </div>
                <div>
                  <span>耗时</span>
                  <strong>{{ cpaTestResult.elapsed_ms }} ms</strong>
                </div>
              </div>
              <div v-if="cpaTestError" class="inline-error">
                {{ cpaTestError }}
              </div>
            </div>

            <label class="setting-toggle compact-toggle">
              <input v-model="autoProbeForm.proxy_enabled" type="checkbox" />
              <span>
                <strong>测活 / 刷新代理</strong>
                <small>测活只请求额度接口，刷新 AT 时也使用该代理</small>
              </span>
            </label>

            <div v-if="autoProbeForm.proxy_enabled" class="proxy-settings">
              <div class="settings-grid">
                <label class="field-label">
                  <span>代理来源</span>
                  <select v-model="autoProbeForm.proxy_mode" class="select">
                    <option value="fixed">固定代理</option>
                    <option value="api">711 / API 拉取</option>
                  </select>
                </label>
                <label class="field-label">
                  <span>默认协议</span>
                  <select v-model="autoProbeForm.proxy_default_scheme" class="select">
                    <option value="http">HTTP</option>
                    <option value="socks5">SOCKS5</option>
                    <option value="socks5h">SOCKS5H</option>
                  </select>
                </label>
              </div>

              <label v-if="autoProbeForm.proxy_mode === 'fixed'" class="field-label full">
                <span>固定代理</span>
                <input
                  v-model="autoProbeForm.proxy_url"
                  class="input"
                  placeholder="http://user:pass@host:port 或 socks5://host:port"
                />
              </label>
              <label v-else class="field-label full">
                <span>代理 API</span>
                <input
                  v-model="autoProbeForm.proxy_api_url"
                  class="input"
                  placeholder="粘贴 711Proxy Get API 链接"
                />
              </label>
              <p class="form-note">API 返回 ip:port 会自动补默认协议；ip:port:user:pass 会自动转为代理 URL 并脱敏显示。</p>
              <div class="toolbar proxy-test-actions">
                <button class="button" :disabled="proxyTestDisabled" @click="testProxyEgress">
                  <Globe :size="15" />测试出口 IP
                </button>
              </div>
              <div v-if="proxyTestResult" class="proxy-test-result">
                <div>
                  <span>出口 IP</span>
                  <strong class="mono">{{ proxyTestResult.ip }}</strong>
                </div>
                <div>
                  <span>模式</span>
                  <strong>{{ proxyTestResult.mode === 'direct' ? '直连' : proxyTestResult.mode === 'api' ? 'API 拉取' : '固定代理' }}</strong>
                </div>
                <div>
                  <span>代理</span>
                  <strong class="mono">{{ proxyTestResult.proxy || '-' }}</strong>
                </div>
                <div>
                  <span>耗时</span>
                  <strong>{{ proxyTestResult.elapsed_ms }} ms</strong>
                </div>
              </div>
              <div v-if="proxyTestError" class="inline-error">
                {{ proxyTestError }}
              </div>
            </div>

            <div class="probe-meta-grid">
              <div>
                <span>下次运行</span>
                <strong>{{ formatTime(autoProbeNextRunAt) }}</strong>
              </div>
              <div>
                <span>上次开始</span>
                <strong>{{ formatTime(autoProbeSettings?.last_started_at) }}</strong>
              </div>
              <div>
                <span>上次完成</span>
                <strong>{{ formatTime(autoProbeSettings?.last_finished_at) }}</strong>
              </div>
              <div>
                <span>上次检测</span>
                <strong>{{ autoProbeSettings?.last_checked_count ?? 0 }} 个</strong>
              </div>
            </div>

            <div v-if="autoProbeSettings?.last_error" class="inline-error">
              {{ autoProbeSettings.last_error }}
            </div>

            <div class="toolbar section-toolbar auto-probe-actions">
              <button class="button primary" :disabled="busy" @click="saveAutoProbeSettings">
                <Save :size="15" />保存设置
              </button>
              <button class="button" :disabled="busy" @click="runAutoProbeNow">
                <Play :size="15" />立即测活
              </button>
            </div>

            <Transition name="fade">
              <pre v-if="autoProbeDisplayResult" class="result mono auto-probe-result">{{ autoProbeDisplayResult }}</pre>
            </Transition>
          </div>
        </div>

        <div class="panel redeem-rate-panel">
          <div class="panel-header">
            <div>
              <h2>兑换限速</h2>
              <p>公网兑换接口请求保护</p>
            </div>
            <span class="badge" :class="redeemRateLimitForm.enabled ? 'available' : 'disabled'">
              {{ redeemRateLimitForm.enabled ? '已启用' : '已关闭' }}
            </span>
          </div>
          <div class="panel-body grid">
            <label class="setting-toggle">
              <input v-model="redeemRateLimitForm.enabled" type="checkbox" />
              <span>
                <strong>启用兑换限速</strong>
                <small>白名单 IP 不受限，其他公网请求按窗口计数</small>
              </span>
            </label>

            <div class="settings-grid">
              <label class="field-label">
                <span>窗口秒数</span>
                <input v-model.number="redeemRateLimitForm.window_seconds" class="input" type="number" min="1" max="86400" />
              </label>
              <label class="field-label">
                <span>窗口请求数</span>
                <input v-model.number="redeemRateLimitForm.max_requests" class="input" type="number" min="1" max="100000" />
              </label>
              <label class="field-label">
                <span>白名单数量</span>
                <input class="input" :value="redeemRateLimitWhitelistCount" readonly />
              </label>
            </div>

            <label class="field-label full">
              <span>白名单 IP</span>
              <textarea
                v-model="redeemRateLimitForm.whitelist_text"
                class="textarea compact-textarea mono"
                spellcheck="false"
                placeholder="一行一个 IP，或用逗号分隔"
              ></textarea>
            </label>
            <p class="form-note">服务端会读取 X-Forwarded-For、X-Real-IP、CF-Connecting-IP；宝塔 Nginx 反代时请保留真实 IP 请求头。</p>

            <div class="probe-meta-grid rate-limit-meta">
              <div>
                <span>当前窗口</span>
                <strong>{{ redeemRateLimitForm.window_seconds || 0 }} 秒</strong>
              </div>
              <div>
                <span>窗口额度</span>
                <strong>{{ redeemRateLimitForm.max_requests || 0 }} 次</strong>
              </div>
              <div>
                <span>白名单</span>
                <strong>{{ redeemRateLimitWhitelistCount }} 个</strong>
              </div>
              <div>
                <span>更新时间</span>
                <strong>{{ formatTime(redeemRateLimitSettings?.updated_at) }}</strong>
              </div>
            </div>

            <div class="toolbar section-toolbar auto-probe-actions">
              <button class="button primary" :disabled="busy" @click="saveRedeemRateLimitSettings">
                <Save :size="15" />保存限速
              </button>
            </div>

            <Transition name="fade">
              <div v-if="redeemRateLimitResult" class="inline-success">
                {{ redeemRateLimitResult }}
              </div>
            </Transition>
          </div>
        </div>

        <div class="panel import-panel">
          <div class="panel-header">
            <div>
              <h2>批量导入</h2>
              <p>CPA / Sub2API / JSONL</p>
            </div>
            <button class="button primary" :disabled="busy || !importText.trim()" @click="importAccounts">
              <Upload :size="15" />导入
            </button>
          </div>
          <div class="panel-body grid">
            <label class="field-label full">
              <span>导入到号池</span>
              <select v-model="importPoolId" class="select">
                <option v-for="pool in activePools" :key="pool.id" :value="pool.id">
                  {{ poolLabel(pool.id) }}
                </option>
              </select>
            </label>
            <textarea v-model="importText" class="textarea" spellcheck="false" placeholder="粘贴 CPA auth JSON / auth 数组 / Sub2API accounts JSON / Codex token JSONL"></textarea>
            <Transition name="fade">
              <pre v-if="adminResult" class="result mono admin-result">{{ adminResult }}</pre>
            </Transition>
          </div>
        </div>
      </div>
    </div>
  </section>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { Activity, Database, Download, Globe, Play, RotateCcw, Save, Search, Trash2, Upload } from 'lucide-vue-next'
import {
  useAdmin,
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
  importAccounts,
  saveAutoProbeSettings,
  saveRedeemRateLimitSettings,
  testProxyEgress,
  testCpaConnection,
  scanCpa401,
  runAutoProbeNow,
  toggleAll,
  poolLabel,
  statusLabel,
  statusBadgeClass,
  canDeleteAccount,
  quotaPercentText,
  quotaPercentBarWidth,
  quotaUsageClass,
  formatTime,
} from '../composables/useAdmin'

const {
  accounts,
  selectedIds,
  importText,
  activePools,
  importPoolId,
  adminResult,
  adminExportFormat,
  autoProbeSettings,
  autoProbeNextRunAt,
  autoProbeResult,
  redeemRateLimitSettings,
  redeemRateLimitResult,
  proxyTestResult,
  proxyTestError,
  cpaTestResult,
  cpaTestError,
  autoProbeForm,
  autoProbeIntervalMinutes,
  redeemRateLimitForm,
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
  proxyTestDisabled,
  cpaTestDisabled,
  cpaScanDisabled,
} = useAdmin()

const autoProbeDisplayResult = computed(() => {
  if (autoProbeResult.value) return autoProbeResult.value
  if (autoProbeSettings.value?.last_result) {
    return JSON.stringify(autoProbeSettings.value.last_result, null, 2)
  }
  return ''
})

const redeemRateLimitWhitelistCount = computed(() => {
  return redeemRateLimitForm.whitelist_text
    .split(/\r?\n|,/)
    .map((value) => value.trim())
    .filter(Boolean).length
})
</script>
