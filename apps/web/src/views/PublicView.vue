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

        <div class="redeem-layout">
          <div class="panel redeem-panel">
            <div class="panel-header">
              <div>
                <h2>兑换码</h2>
                <p>每行一个兑换码，支持批量兑换</p>
              </div>
            </div>
            <div class="panel-body grid">
              <textarea
                v-model="redeemText"
                class="textarea redeem-textarea"
                placeholder="XXXX-XXXX-XXXX-XXXX&#10;YYYY-YYYY-YYYY-YYYY"
                spellcheck="false"
              ></textarea>
              <div class="redeem-actions">
                <select v-model="redeemFormat" class="select">
                  <option value="cpa">CPA JSON</option>
                  <option value="sub2api">Sub2API JSON</option>
                </select>
                <button class="button primary" :disabled="busy || !redeemText.trim()" @click="handleRedeem">
                  <Download :size="15" />
                  <span>兑换并导出</span>
                </button>
              </div>
            </div>
          </div>

          <div class="dark-panel result-panel">
            <div class="panel-header">
              <div>
                <h2>导出结果</h2>
                <p>{{ redeemDownload ? redeemDownload.filename : formatLabel }}</p>
              </div>
              <button class="button on-dark" :disabled="!redeemDocument && !redeemDownload" @click="handleDownload">
                <FileJson :size="15" />下载
              </button>
            </div>
            <div class="panel-body">
              <div class="terminal-bar">
                <span></span><span></span><span></span>
              </div>
              <pre class="result mono dark-result">{{ redeemResultText || previewText }}</pre>
            </div>
          </div>
        </div>
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
import { computed, ref } from 'vue'
import { Download, FileCode, FileJson, ShieldCheck } from 'lucide-vue-next'
import { api, type EncodedDownload, type ExportFormat } from '../api/client'
import { downloadEncodedFile, downloadJson, exportResultForDisplay, timestamp, withBusy } from '../composables/useAdmin'
import { useToast } from '../composables/useToast'
import BrandMark from '../components/BrandMark.vue'

const { success, error: showError } = useToast()

const busy = ref(false)
const redeemText = ref('')
const redeemFormat = ref<ExportFormat>('cpa')
const redeemResultText = ref('')
const redeemDocument = ref<unknown | null>(null)
const redeemDownload = ref<EncodedDownload | null>(null)

const formatLabel = computed(() => redeemFormat.value === 'cpa' ? 'CPA JSON / ZIP' : 'Sub2API JSON')
const previewText = computed(() => redeemFormat.value === 'cpa'
  ? 'CPA 单账号会导出 JSON，多账号会导出 ZIP 包。'
  : 'Sub2API 会导出包含 accounts 的 JSON。')

async function handleRedeem() {
  busy.value = true
  try {
    const codes = redeemText.value.split(/\r?\n/).map((s) => s.trim()).filter(Boolean)
    const result = await api.redeemExport({ codes, format: redeemFormat.value })
    redeemDocument.value = result.document
    redeemDownload.value = result.download || null
    redeemResultText.value = JSON.stringify(exportResultForDisplay(result), null, 2)
    if (result.download) downloadEncodedFile(result.download)
    else downloadJson(`aether-pool-redeem-${result.format}-${timestamp()}.json`, result.document)
    success('兑换成功，文件已开始下载')
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    redeemResultText.value = msg
    showError(msg)
  } finally {
    busy.value = false
  }
}

function handleDownload() {
  if (redeemDownload.value) {
    downloadEncodedFile(redeemDownload.value)
    return
  }
  if (!redeemDocument.value) return
  downloadJson(`aether-pool-redeem-${redeemFormat.value}-${timestamp()}.json`, redeemDocument.value)
}
</script>
