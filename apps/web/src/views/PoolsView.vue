<template>
  <section class="grid">
    <div class="stats">
      <div class="stat dark-stat">
        <span>总号池</span>
        <strong>{{ accountPools.length }}</strong>
      </div>
      <div class="stat">
        <span>启用中</span>
        <strong>{{ activePools.length }}</strong>
      </div>
      <div class="stat">
        <span>已停用</span>
        <strong>{{ inactivePoolCount }}</strong>
      </div>
      <div class="stat">
        <span>默认池</span>
        <strong class="stat-label-value">{{ defaultPoolName }}</strong>
      </div>
    </div>

    <div class="pools-grid">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h2>新建号池</h2>
            <p>所有池都是 Codex 账号池，仅用于库存隔离</p>
          </div>
          <button class="button primary" :disabled="busy || !poolForm.name.trim()" @click="createPool">
            <Plus :size="15" />新建
          </button>
        </div>
        <div class="panel-body grid">
          <div class="settings-grid">
            <label class="field-label">
              <span>名称</span>
              <input v-model="poolForm.name" class="input" placeholder="例如 Team US Plus" />
            </label>
            <label class="field-label">
              <span>工作区</span>
              <input v-model="poolForm.workspace_label" class="input" placeholder="例如 workspace/team" />
            </label>
            <label class="field-label">
              <span>类型</span>
              <input v-model="poolForm.account_type" class="input" placeholder="codex" />
            </label>
          </div>
          <label class="field-label full">
            <span>备注</span>
            <input v-model="poolForm.description" class="input" placeholder="可选" />
          </label>
          <Transition name="fade">
            <pre v-if="adminResult" class="result mono admin-result">{{ adminResult }}</pre>
          </Transition>
        </div>
      </div>

      <div class="panel pools-list-panel">
        <div class="panel-header">
          <div>
            <h2>号池列表</h2>
            <p>停用后不再用于新导入和新批次</p>
          </div>
          <button class="button" :disabled="busy" @click="loadPools">
            <RefreshCw :size="15" :class="{ spinning: busy }" />刷新
          </button>
        </div>
        <div class="panel-body grid">
          <div class="pool-list">
            <div v-for="pool in accountPools" :key="pool.id" class="pool-row">
              <div>
                <strong>{{ pool.name }}</strong>
                <span class="muted">{{ pool.workspace_label || '-' }} / {{ pool.account_type || 'codex' }}</span>
                <span class="muted mono">{{ pool.id }}</span>
              </div>
              <div class="toolbar compact-toolbar">
                <span v-if="pool.is_default" class="badge available">默认</span>
                <span class="badge" :class="pool.is_active ? 'available' : 'disabled'">
                  {{ pool.is_active ? '启用' : '停用' }}
                </span>
                <button class="button ghost tiny" :disabled="busy || pool.is_default" @click="togglePoolActive(pool)">
                  {{ pool.is_active ? '停用' : '启用' }}
                </button>
              </div>
            </div>
            <div v-if="!accountPools.length" class="empty-row pool-empty">
              <Database :size="20" />
              <span>暂无号池</span>
            </div>
          </div>
        </div>
      </div>

      <div class="panel pools-api-panel">
        <div class="panel-header">
          <div>
            <h2>注册机接口</h2>
            <p>上传前先查询启用号池，再带 pool_id 导入账号</p>
          </div>
        </div>
        <div class="panel-body grid">
          <div class="endpoint-list">
            <div>
              <span>查询启用号池</span>
              <strong class="mono">GET /api/alalalateam/account-pools?active_only=true</strong>
            </div>
            <div>
              <span>上传账号</span>
              <strong class="mono">POST /api/alalalateam/accounts/import</strong>
            </div>
          </div>
          <p class="form-note">两个接口都使用后台 Authorization Bearer token；上传 JSON body 增加 pool_id。</p>
        </div>
      </div>
    </div>
  </section>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { Database, Plus, RefreshCw } from 'lucide-vue-next'
import {
  useAdmin,
  createPool,
  loadPools,
  togglePoolActive,
} from '../composables/useAdmin'

const {
  accountPools,
  activePools,
  adminResult,
  busy,
  defaultPoolId,
  poolForm,
} = useAdmin()

const inactivePoolCount = computed(() => Math.max(accountPools.value.length - activePools.value.length, 0))
const defaultPoolName = computed(() => {
  const pool = accountPools.value.find((item) => item.id === defaultPoolId.value || item.is_default)
  return pool?.name || '-'
})
</script>
