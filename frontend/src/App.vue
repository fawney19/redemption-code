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
  isManagementPath,
  loadPools,
  loadAccounts,
  loadBatches,
  loadAutoProbeSettings,
  loadRedeemRateLimitSettings,
  logoutAdmin,
  syncAdminViewFromLocation,
} from './composables/useAdmin'
import PublicView from './views/PublicView.vue'
import AdminLogin from './views/AdminLogin.vue'
import AdminDashboard from './views/AdminDashboard.vue'
import AppToast from './components/AppToast.vue'

const { adminAuthenticated, adminTokenDraft, adminToken, adminResult } = useAdmin()
const isAdminRoute = ref(isManagementPath())

function syncRoute() {
  isAdminRoute.value = isManagementPath()
  if (isAdminRoute.value) syncAdminViewFromLocation()
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
