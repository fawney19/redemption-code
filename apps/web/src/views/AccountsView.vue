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
                <tr v-if="!accounts.length">
                  <td colspan="8" class="empty-row">
                    <Database :size="20" />
                    <span>暂无账号数据</span>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
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
  </section>
</template>

<script setup lang="ts">
import { Activity, Database, Download, RotateCcw, Search, Upload } from 'lucide-vue-next'
import {
  useAdmin,
  loadAccounts,
  probeSelected,
  refreshSelected,
  exportSelected,
  importAccounts,
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
  filters,
  busy,
  availableCount,
  redeemedCount,
  attentionCount,
  allSelected,
} = useAdmin()
</script>
