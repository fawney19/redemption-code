<template>
  <section class="grid">
    <div class="stats">
      <div class="stat dark-stat">
        <span>总账号</span>
        <strong>{{ accounts.length }}</strong>
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
        <span>需要处理</span>
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
            <input v-model="filters.search" class="input search-input" placeholder="搜索邮箱 / Account ID" @keyup.enter="loadAccounts" />
            <select v-model="filters.status" class="select status-select" @change="loadAccounts">
              <option value="">全部状态</option>
              <option value="available">可用</option>
              <option value="at_expired">AT 过期</option>
              <option value="refresh_failed">刷新失败</option>
              <option value="quota_exhausted">额度耗尽</option>
              <option value="auth_invalid">账号失效</option>
              <option value="forbidden">网络受限</option>
              <option value="redeemed">已兑换</option>
            </select>
            <button class="button" @click="loadAccounts"><Search :size="15" />查询</button>
          </div>
        </div>
        <div class="panel-body">
          <div class="toolbar section-toolbar">
            <button class="button" :disabled="busy" @click="probeSelected"><Activity :size="15" />测活</button>
            <button class="button" :disabled="busy" @click="refreshSelected"><RotateCcw :size="15" />刷新 AT</button>
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
                  <th>状态</th>
                  <th>绑定兑换码</th>
                  <th>AT</th>
                  <th>RT</th>
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
                  <td><span class="badge" :class="account.status">{{ statusLabel(account.status) }}</span></td>
                  <td class="mono">{{ account.redeem_code_masked || '-' }}</td>
                  <td class="mono">{{ account.access_token_preview || '-' }}</td>
                  <td class="mono">{{ account.refresh_token_preview || '-' }}</td>
                  <td>{{ formatTime(account.expires_at) }}</td>
                  <td>{{ formatTime(account.last_probe_at) }}</td>
                  <td>
                    <button class="button ghost tiny" :disabled="busy" @click="probeAccount(account.id)">
                      <Activity :size="14" />测活
                    </button>
                  </td>
                </tr>
                <tr v-if="!accounts.length">
                  <td colspan="9" class="empty-row">
                    <Database :size="20" />
                    <span>暂无账号数据</span>
                  </td>
                </tr>
              </tbody>
            </table>
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

            <label class="setting-toggle compact-toggle">
              <input v-model="autoProbeForm.refresh_before_probe" type="checkbox" />
              <span>
                <strong>测活前刷新 AT</strong>
                <small>过期或接近过期时刷新，已兑换账号跳过</small>
              </span>
            </label>

            <label class="setting-toggle compact-toggle">
              <input v-model="autoProbeForm.proxy_enabled" type="checkbox" />
              <span>
                <strong>测活代理</strong>
                <small>手动测活、单账号测活、自动测活共用</small>
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
          <div class="panel-body">
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
import { Activity, Database, Download, Play, RotateCcw, Save, Search, Upload } from 'lucide-vue-next'
import {
  useAdmin,
  loadAccounts,
  probeSelected,
  probeAccount,
  refreshSelected,
  exportSelected,
  importAccounts,
  saveAutoProbeSettings,
  runAutoProbeNow,
  toggleAll,
  statusLabel,
  formatTime,
} from '../composables/useAdmin'

const {
  accounts,
  selectedIds,
  importText,
  adminResult,
  adminExportFormat,
  autoProbeSettings,
  autoProbeNextRunAt,
  autoProbeResult,
  autoProbeForm,
  autoProbeIntervalMinutes,
  filters,
  busy,
  availableCount,
  redeemedCount,
  attentionCount,
  allSelected,
} = useAdmin()

const autoProbeDisplayResult = computed(() => {
  if (autoProbeResult.value) return autoProbeResult.value
  if (autoProbeSettings.value?.last_result) {
    return JSON.stringify(autoProbeSettings.value.last_result, null, 2)
  }
  return ''
})
</script>
