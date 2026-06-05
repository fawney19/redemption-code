<template>
  <div class="app-shell">
    <aside class="sidebar">
      <BrandMark subtitle="Pool Console" />
      <nav class="sidebar-nav">
        <button class="nav-button" :class="{ active: activeView === 'accounts' }" @click="setActiveView('accounts')">
          <Database :size="16" />账号池
        </button>
        <button class="nav-button" :class="{ active: activeView === 'codes' }" @click="setActiveView('codes')">
          <Ticket :size="16" />兑换码
        </button>
      </nav>
      <button class="nav-button danger" @click="handleLogout">
        <LogOut :size="16" />退出后台
      </button>
    </aside>

    <main class="main">
      <div class="topbar admin-topbar">
        <div>
          <h1>{{ pageTitle }}</h1>
          <p>{{ pageSubtitle }}</p>
        </div>
        <div class="token-box">
          <select v-model="selectedPoolId" class="select pool-select" :disabled="busy" @change="changeSelectedPool">
            <option value="">全部号池</option>
            <option v-for="pool in accountPools" :key="pool.id" :value="pool.id">
              {{ poolLabel(pool.id) }}{{ pool.is_active ? '' : '（停用）' }}
            </option>
          </select>
          <button class="button" :disabled="busy" @click="refreshAdmin">
            <RefreshCw :size="15" :class="{ spinning: busy }" />刷新
          </button>
        </div>
      </div>

      <Transition name="view" mode="out-in">
        <AccountsView v-if="activeView === 'accounts'" />
        <CodesView v-else />
      </Transition>
    </main>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { Database, LogOut, RefreshCw, Ticket } from 'lucide-vue-next'
import { useAdmin, logoutAdmin, refreshAdmin, changeSelectedPool, poolLabel, setActiveView } from '../composables/useAdmin'
import BrandMark from '../components/BrandMark.vue'
import AccountsView from './AccountsView.vue'
import CodesView from './CodesView.vue'

const { activeView, busy, accountPools, selectedPoolId, selectedPoolLabel } = useAdmin()

const pageTitle = computed(() => activeView.value === 'accounts' ? 'Codex 账号池' : '兑换码管理')
const pageSubtitle = computed(() => activeView.value === 'accounts'
  ? `上传账号、刷新 AT、测活并按 CPA/Sub2API 格式导出 · ${selectedPoolLabel.value}`
  : `生成独占兑换码，兑换后账号保留但不再分配 · ${selectedPoolLabel.value}`)

function handleLogout() {
  logoutAdmin()
}
</script>
