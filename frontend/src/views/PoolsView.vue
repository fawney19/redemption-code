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
      <div class="panel pools-list-panel">
        <div class="panel-header">
          <div>
            <h2>号池列表</h2>
            <p>停用后不再用于新导入和新批次</p>
          </div>
          <div class="toolbar compact-toolbar">
            <button class="button primary" :class="{ active: createPoolOpen }" :disabled="busy" @click="createPoolOpen = !createPoolOpen">
              <Plus :size="15" />新建号池
            </button>
            <button class="button" :disabled="busy" @click="loadPools">
              <RefreshCw :size="15" :class="{ spinning: busy }" />刷新
            </button>
          </div>
        </div>
        <div class="panel-body grid">
          <Transition name="fade">
            <div v-if="createPoolOpen" class="pool-create-panel">
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
              <div class="pool-create-footer">
                <label class="field-label">
                  <span>备注</span>
                  <input v-model="poolForm.description" class="input" placeholder="可选" />
                </label>
                <div class="pool-create-actions">
                  <button class="button primary" :disabled="busy || !poolForm.name.trim()" @click="handleCreatePool">
                    <Plus :size="15" />创建
                  </button>
                  <button class="button ghost" :disabled="busy" @click="createPoolOpen = false">收起</button>
                </div>
              </div>
            </div>
          </Transition>

          <Transition name="fade">
            <pre v-if="adminResult" class="result mono admin-result">{{ adminResult }}</pre>
          </Transition>

          <div class="table-wrap">
            <table class="table pool-table">
              <thead>
                <tr>
                  <th>号池</th>
                  <th>工作区</th>
                  <th>类型</th>
                  <th>备注</th>
                  <th>状态</th>
                  <th>操作</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="pool in accountPools" :key="pool.id">
                  <td>
                    <strong>{{ pool.name }}</strong>
                    <div class="muted mono">{{ pool.id }}</div>
                  </td>
                  <td>{{ pool.workspace_label || '-' }}</td>
                  <td class="mono">{{ pool.account_type || 'codex' }}</td>
                  <td>
                    <span class="pool-description">{{ pool.description || '-' }}</span>
                  </td>
                  <td>
                    <div class="pool-status-stack">
                      <span v-if="pool.is_default" class="badge available">默认</span>
                      <span class="badge" :class="pool.is_active ? 'available' : 'disabled'">
                        {{ pool.is_active ? '启用' : '停用' }}
                      </span>
                    </div>
                  </td>
                  <td>
                    <button class="button ghost tiny" :disabled="busy || pool.is_default" @click="togglePoolActive(pool)">
                      {{ pool.is_active ? '停用' : '启用' }}
                    </button>
                  </td>
                </tr>
                <tr v-if="!accountPools.length">
                  <td colspan="6" class="empty-row">
                    <Database :size="20" />
                    <span>暂无号池</span>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </div>
      </div>
    </div>
  </section>
</template>

<script setup lang="ts">
import { computed, ref } from 'vue'
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
const createPoolOpen = ref(false)

const inactivePoolCount = computed(() => Math.max(accountPools.value.length - activePools.value.length, 0))
const defaultPoolName = computed(() => {
  const pool = accountPools.value.find((item) => item.id === defaultPoolId.value || item.is_default)
  return pool?.name || '-'
})

async function handleCreatePool() {
  await createPool()
  createPoolOpen.value = false
}
</script>
