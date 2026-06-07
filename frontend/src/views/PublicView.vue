<template>
  <div class="public-shell">
    <header class="public-header">
      <BrandMark subtitle="Codex 号池服务" />
    </header>

    <main class="public-main">
      <section class="public-hero">
        <div class="public-copy">
          <h1>账号兑换导出</h1>
          <p>批量提交兑换码，选择 CPA 或 Sub2API 格式后直接下载账号文件。无需登录，一步完成。</p>
          <div class="public-meta">
            <span><FileJson :size="14" />CPA 多账号 ZIP</span>
            <span><FileCode :size="14" />Sub2API JSON</span>
            <span><ShieldCheck :size="14" />兑换快照保留</span>
          </div>
        </div>

        <RedemptionWorkbench />
      </section>
    </main>

    <footer class="public-footer">
      <div class="footer-inner">
        <BrandMark subtitle="Codex 号池服务" compact />
        <p>安全可靠的 Codex 账号池管理与分发服务</p>
      </div>
    </footer>
  </div>
</template>

<script setup lang="ts">
import { FileCode, FileJson, ShieldCheck } from 'lucide-vue-next'
import BrandMark from '../components/BrandMark.vue'
import RedemptionWorkbench from '../components/RedemptionWorkbench.vue'
</script>
