<template>
  <PublicView v-if="!isAdminRoute" />
  <AdminLogin v-else-if="!adminAuthenticated" />
  <AdminDashboard v-else />
  <AppToast />
</template>

<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import { useAdmin, loadAccounts, loadBatches, loadAutoProbeSettings } from './composables/useAdmin'
import PublicView from './views/PublicView.vue'
import AdminLogin from './views/AdminLogin.vue'
import AdminDashboard from './views/AdminDashboard.vue'
import AppToast from './components/AppToast.vue'

const { adminAuthenticated, adminTokenDraft, adminToken, adminResult } = useAdmin()
const isAdminRoute = ref(window.location.pathname === '/admin')

function syncRoute() {
  isAdminRoute.value = window.location.pathname === '/admin'
}

onMounted(() => {
  syncRoute()
  window.addEventListener('popstate', syncRoute)
  if (isAdminRoute.value && adminAuthenticated.value) {
    adminTokenDraft.value = adminToken.value
    loadAccounts().catch((e) => {
      adminResult.value = e instanceof Error ? e.message : String(e)
    })
    loadBatches().catch(() => undefined)
    loadAutoProbeSettings().catch(() => undefined)
  }
})

onUnmounted(() => {
  window.removeEventListener('popstate', syncRoute)
})
</script>
