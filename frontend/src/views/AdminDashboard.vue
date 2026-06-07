<template>
  <div class="app-shell">
    <aside class="sidebar">
      <BrandMark subtitle="Pool Console" />
      <nav class="sidebar-nav">
        <a
          class="nav-button"
          :class="{ active: activeView === 'accounts' }"
          :href="adminViewHref('accounts')"
          @click="handleNavClick($event, 'accounts')"
        >
          <Database :size="16" />账号管理
        </a>
        <a
          class="nav-button"
          :class="{ active: activeView === 'codes' }"
          :href="adminViewHref('codes')"
          @click="handleNavClick($event, 'codes')"
        >
          <Ticket :size="16" />兑换码
        </a>
        <a
          class="nav-button"
          :class="{ active: activeView === 'redeem' }"
          :href="adminViewHref('redeem')"
          @click="handleNavClick($event, 'redeem')"
        >
          <Download :size="16" />兑换页
        </a>
        <a
          class="nav-button"
          :class="{ active: activeView === 'pools' }"
          :href="adminViewHref('pools')"
          @click="handleNavClick($event, 'pools')"
        >
          <Database :size="16" />号池管理
        </a>
        <a
          class="nav-button"
          :class="{ active: activeView === 'settings' }"
          :href="adminViewHref('settings')"
          @click="handleNavClick($event, 'settings')"
        >
          <Settings :size="16" />设置
        </a>
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
      </div>

      <Transition name="view" mode="out-in">
        <AccountsView v-if="activeView === 'accounts'" />
        <CodesView v-else-if="activeView === 'codes'" />
        <AdminRedeemView v-else-if="activeView === 'redeem'" />
        <PoolsView v-else-if="activeView === 'pools'" />
        <SettingsView v-else />
      </Transition>
    </main>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { Database, Download, LogOut, Settings, Ticket } from 'lucide-vue-next'
import { useAdmin, logoutAdmin, setActiveView, adminViewHref } from '../composables/useAdmin'
import BrandMark from '../components/BrandMark.vue'
import AccountsView from './AccountsView.vue'
import AdminRedeemView from './AdminRedeemView.vue'
import CodesView from './CodesView.vue'
import PoolsView from './PoolsView.vue'
import SettingsView from './SettingsView.vue'

const { activeView } = useAdmin()

const pageTitle = computed(() => {
  if (activeView.value === 'accounts') return '账号管理'
  if (activeView.value === 'codes') return '兑换码管理'
  if (activeView.value === 'redeem') return '兑换页'
  if (activeView.value === 'pools') return '号池管理'
  return '系统设置'
})
const pageSubtitle = computed(() => {
  if (activeView.value === 'accounts') return '查看账号、刷新 AT、测活并按 CPA/Sub2API 格式导出'
  if (activeView.value === 'codes') return '生成独占兑换码，兑换后账号保留但不再分配'
  if (activeView.value === 'redeem') return '在后台直接提交兑换码并下载账号文件'
  if (activeView.value === 'pools') return '创建、启停和查看 Codex 号池，用于库存隔离'
  return '集中管理自动测活、兑换限速与代理配置'
})

function handleLogout() {
  logoutAdmin()
}

function handleNavClick(event: MouseEvent, view: 'accounts' | 'codes' | 'redeem' | 'pools' | 'settings') {
  if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || event.button !== 0) return
  event.preventDefault()
  setActiveView(view)
}
</script>
