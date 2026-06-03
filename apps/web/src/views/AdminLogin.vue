<template>
  <div class="admin-login-shell">
    <div class="login-panel panel">
      <div class="panel-header">
        <BrandMark subtitle="管理后台" compact />
      </div>
      <div class="panel-body grid">
        <div class="login-hint">
          <Lock :size="15" />
          <span>请输入密码继续</span>
        </div>
        <input
          v-model="adminTokenDraft"
          class="input"
          type="password"
          autocomplete="current-password"
          placeholder="密码"
          @keyup.enter="handleLogin"
        />
        <button class="button primary" :disabled="busy || !adminTokenDraft.trim()" @click="handleLogin">
          <LogIn :size="15" />进入管理后台
        </button>
        <Transition name="fade">
          <pre v-if="adminResult" class="result mono">{{ adminResult }}</pre>
        </Transition>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { Lock, LogIn } from 'lucide-vue-next'
import { useAdmin, loginAdmin } from '../composables/useAdmin'
import BrandMark from '../components/BrandMark.vue'

const { adminTokenDraft, busy, adminResult } = useAdmin()

async function handleLogin() {
  await loginAdmin()
}
</script>
