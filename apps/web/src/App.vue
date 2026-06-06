<template>
  <PublicView v-if="!isAdminRoute" />
  <AdminLogin v-else-if="!adminAuthenticated" />
  <AdminDashboard v-else />
  <AppToast />
</template>

<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import {
  useAdmin,
  loadPools,
  loadAccounts,
  loadBatches,
  loadAutoProbeSettings,
  loadRedeemRateLimitSettings,
  logoutAdmin,
} from './composables/useAdmin'
import PublicView from './views/PublicView.vue'
import AdminLogin from './views/AdminLogin.vue'
import AdminDashboard from './views/AdminDashboard.vue'
import AppToast from './components/AppToast.vue'

const { adminAuthenticated, adminTokenDraft, adminToken, adminResult } = useAdmin()
const MANAGEMENT_ENTRY_PATH = '/alalalateam'
const isAdminRoute = ref(isManagementEntryPath())

function syncRoute() {
  isAdminRoute.value = isManagementEntryPath()
}

function isManagementEntryPath() {
  return window.location.pathname.replace(/\/+$/, '') === MANAGEMENT_ENTRY_PATH
}

onMounted(() => {
  syncRoute()
  window.addEventListener('popstate', syncRoute)
  if (isAdminRoute.value && adminAuthenticated.value) {
    adminTokenDraft.value = adminToken.value
    loadPools()
      .then(() => Promise.all([
        loadAccounts(),
        loadBatches(),
        loadAutoProbeSettings(),
        loadRedeemRateLimitSettings(),
      ]))
      .catch((e) => {
        const message = e instanceof Error ? e.message : String(e)
        if (message.toLowerCase().includes('unauthorized')) {
          logoutAdmin()
          adminResult.value = '登录已失效，请重新输入密码'
          return
        }
        adminResult.value = message
      })
  }
})

onUnmounted(() => {
  window.removeEventListener('popstate', syncRoute)
})
</script>
