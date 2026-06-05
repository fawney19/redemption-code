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
                <h2>{{ modeTitle }}</h2>
                <p>{{ modeSubtitle }}</p>
              </div>
            </div>
            <div class="panel-body grid">
              <div class="mode-tabs" role="tablist" aria-label="兑换模式">
                <button
                  class="mode-tab"
                  :class="{ active: redeemMode === 'redeem' }"
                  type="button"
                  @click="redeemMode = 'redeem'"
                >
                  <Download :size="15" />
                  <span>兑换导出</span>
                </button>
                <button
                  class="mode-tab"
                  :class="{ active: redeemMode === 'afterSale' }"
                  type="button"
                  @click="redeemMode = 'afterSale'"
                >
                  <RefreshCw :size="15" />
                  <span>售后补发</span>
                </button>
              </div>
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
                  <component :is="redeemMode === 'afterSale' ? RefreshCw : Download" :size="15" />
                  <span>{{ actionLabel }}</span>
                </button>
              </div>
            </div>
          </div>

          <div class="dark-panel result-panel">
            <div class="panel-header">
              <div>
                <h2>导出结果</h2>
                <p>{{ redeemFileName || formatLabel }}</p>
              </div>
              <button class="button on-dark" :disabled="!redeemDocument && !redeemDownload" @click="handleDownload">
                <FileJson :size="15" />下载
              </button>
            </div>
            <div class="panel-body result-summary-body">
              <div v-if="redeemStatus === 'idle'" class="result-empty">
                {{ previewText }}
              </div>

              <div v-else-if="redeemStatus === 'success'" class="result-summary">
                <div class="result-status success">
                  <CircleCheck :size="18" />
                  <span>导出文件已生成</span>
                </div>
                <div class="result-metrics">
                  <div>
                    <span>{{ successMetricLabel }}</span>
                    <strong>{{ redeemSuccesses.length }}</strong>
                  </div>
                  <div>
                    <span>{{ accountMetricLabel }}</span>
                    <strong>{{ redeemedAccountCount }}</strong>
                  </div>
                  <div>
                    <span>失败兑换码</span>
                    <strong>{{ redeemFailures.length }}</strong>
                  </div>
                </div>
                <div class="download-summary">
                  <span>文件</span>
                  <strong>{{ redeemFileName || `${formatLabel} 文件` }}</strong>
                </div>
                <div v-if="redeemFailures.length" class="failure-list">
                  <div v-for="failure in redeemFailures" :key="failure.code" class="failure-item">
                    <span class="mono">{{ failure.code }}</span>
                    <strong>{{ failure.reason }}</strong>
                  </div>
                </div>
              </div>

              <div v-else class="result-summary">
                <div class="result-status error">
                  <CircleAlert :size="18" />
                  <span>兑换失败</span>
                </div>
                <div class="inline-dark-error">{{ redeemErrorText }}</div>
              </div>
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
import { CircleAlert, CircleCheck, Download, FileCode, FileJson, RefreshCw, ShieldCheck } from 'lucide-vue-next'
import { api, type EncodedDownload, type ExportFormat, type RedeemFailure, type RedeemSuccess } from '../api/client'
import { downloadEncodedFile, downloadJson, normalizeDownloadFileName, timestamp } from '../composables/useAdmin'
import { useToast } from '../composables/useToast'
import BrandMark from '../components/BrandMark.vue'

const { success, error: showError } = useToast()

const busy = ref(false)
const redeemMode = ref<'redeem' | 'afterSale'>('redeem')
const redeemText = ref('')
const redeemFormat = ref<ExportFormat>('cpa')
const redeemStatus = ref<'idle' | 'success' | 'error'>('idle')
const redeemErrorText = ref('')
const redeemDocument = ref<unknown | null>(null)
const redeemDownload = ref<EncodedDownload | null>(null)
const redeemFileName = ref('')
const redeemSuccesses = ref<RedeemSuccess[]>([])
const redeemFailures = ref<RedeemFailure[]>([])
const maxRedeemCodes = 500

const formatLabel = computed(() => redeemFormat.value === 'cpa' ? 'CPA JSON / ZIP' : 'Sub2API JSON')
const modeTitle = computed(() => redeemMode.value === 'afterSale' ? '售后补发' : '兑换码')
const modeSubtitle = computed(() => redeemMode.value === 'afterSale' ? '提交已兑换码，失效后自动补发' : '每行一个兑换码，支持批量兑换')
const actionLabel = computed(() => redeemMode.value === 'afterSale' ? '售后并导出' : '兑换并导出')
const successMetricLabel = computed(() => redeemMode.value === 'afterSale' ? '成功售后' : '成功兑换')
const accountMetricLabel = computed(() => redeemMode.value === 'afterSale' ? '补发账号' : '导出账号')
const previewText = computed(() => {
  if (redeemMode.value === 'afterSale') return '仅认证失效类账号会自动补发。'
  return redeemFormat.value === 'cpa'
    ? 'CPA 单账号会导出 JSON，多账号会导出 ZIP 包。'
    : 'Sub2API 会导出包含 accounts 的 JSON。'
})
const redeemedAccountCount = computed(() => redeemSuccesses.value.reduce((sum, item) => sum + Number(item.account_count || 0), 0))

async function handleRedeem() {
  busy.value = true
  try {
    const codes = redeemText.value.split(/\r?\n/).map((s) => s.trim()).filter(Boolean)
    if (codes.length > maxRedeemCodes) {
      throw new Error(`单次最多提交 ${maxRedeemCodes} 个兑换码`)
    }
    const result = redeemMode.value === 'afterSale'
      ? await api.redeemAfterSale({ codes, format: redeemFormat.value })
      : await api.redeemExport({ codes, format: redeemFormat.value })
    const prefix = redeemMode.value === 'afterSale' ? 'after-sale' : 'redeem'
    const fallbackFileName = `account-pool-${prefix}-${result.format}-${timestamp()}.json`
    redeemDocument.value = result.document
    redeemDownload.value = result.download || null
    redeemFileName.value = result.download?.filename
      ? normalizeDownloadFileName(result.download.filename)
      : fallbackFileName
    redeemSuccesses.value = result.successes
    redeemFailures.value = result.failures
    redeemStatus.value = 'success'
    redeemErrorText.value = ''
    if (result.download) downloadEncodedFile(result.download)
    else downloadJson(fallbackFileName, result.document)
    success(redeemMode.value === 'afterSale' ? '售后成功，文件已开始下载' : '兑换成功，文件已开始下载')
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    redeemStatus.value = 'error'
    redeemErrorText.value = msg
    redeemDocument.value = null
    redeemDownload.value = null
    redeemFileName.value = ''
    redeemSuccesses.value = []
    redeemFailures.value = []
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
  downloadJson(redeemFileName.value || `account-pool-redeem-${redeemFormat.value}-${timestamp()}.json`, redeemDocument.value)
}
</script>
