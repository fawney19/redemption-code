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
              <div class="panel-actions">
                <button class="button on-dark" :disabled="!redeemDocument && !redeemDownload" @click="handleDownload">
                  <Download :size="15" />账号文件
                </button>
                <button
                  v-if="redeemFailures.length"
                  class="button on-dark"
                  type="button"
                  @click="handleDownloadFailures"
                >
                  <Download :size="15" />失败清单
                </button>
              </div>
            </div>
            <div class="panel-body result-summary-body">
              <div v-if="redeemStatus === 'idle'" class="result-empty">
                {{ previewText }}
              </div>

              <div v-else-if="redeemStatus === 'running'" class="result-summary">
                <div class="result-status">
                  <RefreshCw :size="18" />
                  <span>{{ redeemJob?.message || '正在兑换' }}</span>
                </div>
                <div class="result-metrics">
                  <div>
                    <span>已处理</span>
                    <strong>{{ redeemJob?.processed_codes || 0 }}</strong>
                  </div>
                  <div>
                    <span>总兑换码</span>
                    <strong>{{ redeemJob?.total_codes || 0 }}</strong>
                  </div>
                  <div>
                    <span>进度</span>
                    <strong>{{ redeemProgress }}%</strong>
                  </div>
                </div>
                <div class="redeem-progress-track">
                  <div class="redeem-progress-fill" :style="{ width: `${redeemProgress}%` }"></div>
                </div>
                <div class="download-summary">
                  <span>成功 / 失败</span>
                  <strong>{{ redeemJob?.success_count || 0 }} / {{ redeemJob?.failure_count || 0 }}</strong>
                </div>
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
                  <div v-for="failure in visibleRedeemFailures" :key="failure.code" class="failure-item">
                    <span class="mono">{{ failure.code }}</span>
                    <strong>{{ failure.reason }}</strong>
                  </div>
                  <div v-if="hiddenRedeemFailureCount" class="failure-item">
                    <span class="mono">...</span>
                    <strong>还有 {{ hiddenRedeemFailureCount }} 条失败记录未展示</strong>
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
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { CircleAlert, CircleCheck, Download, FileCode, FileJson, RefreshCw, ShieldCheck } from 'lucide-vue-next'
import { api, type EncodedDownload, type ExportFormat, type RedeemFailure, type RedeemJob, type RedeemSuccess } from '../api/client'
import { downloadEncodedFile, downloadJson, downloadText, normalizeDownloadFileName, timestamp } from '../composables/useAdmin'
import { useToast } from '../composables/useToast'
import BrandMark from '../components/BrandMark.vue'

const { success, error: showError } = useToast()

const redeemJobStorageKey = 'aether-pool.redeem-job'

const busy = ref(false)
const redeemMode = ref<'redeem' | 'afterSale'>('redeem')
const redeemText = ref('')
const redeemFormat = ref<ExportFormat>('cpa')
const redeemStatus = ref<'idle' | 'running' | 'success' | 'error'>('idle')
const redeemErrorText = ref('')
const redeemJob = ref<RedeemJob | null>(null)
const redeemDocument = ref<unknown | null>(null)
const redeemDownload = ref<EncodedDownload | null>(null)
const redeemFileName = ref('')
const redeemSuccesses = ref<RedeemSuccess[]>([])
const redeemFailures = ref<RedeemFailure[]>([])
let redeemPollTimer: number | null = null

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
const redeemProgress = computed(() => Math.max(0, Math.min(100, Math.round(Number(redeemJob.value?.progress || 0)))))
const visibleRedeemFailures = computed(() => redeemFailures.value.slice(0, 100))
const hiddenRedeemFailureCount = computed(() => Math.max(0, redeemFailures.value.length - visibleRedeemFailures.value.length))

async function handleRedeem() {
  clearRedeemPoll()
  busy.value = true
  try {
    const codes = redeemText.value.split(/\r?\n/).map((s) => s.trim()).filter(Boolean)
    const response = redeemMode.value === 'afterSale'
      ? await api.startRedeemAfterSaleJob({ codes, format: redeemFormat.value })
      : await api.startRedeemExportJob({ codes, format: redeemFormat.value })
    applyRedeemJob(response.job)
    storeRedeemJob(response.job)
    redeemStatus.value = 'running'
    redeemErrorText.value = ''
    redeemDocument.value = null
    redeemDownload.value = null
    redeemFileName.value = ''
    redeemSuccesses.value = []
    redeemFailures.value = []
    scheduleRedeemPoll(response.job.id)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    clearRedeemPoll()
    clearStoredRedeemJob()
    busy.value = false
    redeemStatus.value = 'error'
    redeemErrorText.value = msg
    redeemJob.value = null
    redeemDocument.value = null
    redeemDownload.value = null
    redeemFileName.value = ''
    redeemSuccesses.value = []
    redeemFailures.value = []
    showError(msg)
  }
}

function scheduleRedeemPoll(jobId: string) {
  redeemPollTimer = window.setTimeout(() => {
    void pollRedeemJob(jobId)
  }, 700)
}

async function pollRedeemJob(jobId: string) {
  try {
    const response = await api.getRedeemJob(jobId)
    applyRedeemJob(response.job)
    storeRedeemJob(response.job)
    if (response.job.status === 'completed') {
      finishRedeemJob(response.job)
      return
    }
    if (response.job.status === 'failed') {
      throw new Error(response.job.error || '兑换任务失败')
    }
    scheduleRedeemPoll(jobId)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    clearRedeemPoll()
    clearStoredRedeemJob()
    busy.value = false
    redeemStatus.value = 'error'
    redeemErrorText.value = msg
    showError(msg)
  }
}

function applyRedeemJob(job: RedeemJob) {
  redeemJob.value = job
  redeemFormat.value = job.format
  if (job.mode === 'after_sale') redeemMode.value = 'afterSale'
  else if (job.mode === 'redeem') redeemMode.value = 'redeem'
}

function finishRedeemJob(job: RedeemJob) {
  clearRedeemPoll()
  clearStoredRedeemJob()
  const result = job.result
  if (!result) {
    throw new Error('兑换任务结果为空')
  }
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
  busy.value = false
  if (result.download) downloadEncodedFile(result.download)
  else downloadJson(fallbackFileName, result.document)
  success(redeemMode.value === 'afterSale' ? '售后成功，文件已开始下载' : '兑换成功，文件已开始下载')
}

function clearRedeemPoll() {
  if (redeemPollTimer !== null) {
    window.clearTimeout(redeemPollTimer)
    redeemPollTimer = null
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

function handleDownloadFailures() {
  if (!redeemFailures.value.length) return
  const prefix = redeemMode.value === 'afterSale' ? 'after-sale' : 'redeem'
  const lines = [
    'code\treason',
    ...redeemFailures.value.map((failure) => `${failure.code}\t${failure.reason}`),
  ]
  downloadText(`account-pool-${prefix}-failures-${timestamp()}.txt`, lines.join('\n'))
}

async function resumeStoredRedeemJob() {
  const jobId = readStoredRedeemJobId()
  if (!jobId) return
  try {
    const response = await api.getRedeemJob(jobId)
    applyRedeemJob(response.job)
    if (response.job.status === 'completed') {
      finishRedeemJob(response.job)
      return
    }
    if (response.job.status === 'failed') {
      clearStoredRedeemJob()
      redeemStatus.value = 'error'
      redeemErrorText.value = response.job.error || '兑换任务失败'
      return
    }
    busy.value = true
    redeemStatus.value = 'running'
    redeemErrorText.value = ''
    scheduleRedeemPoll(response.job.id)
  } catch {
    clearStoredRedeemJob()
  }
}

function storeRedeemJob(job: RedeemJob) {
  try {
    localStorage.setItem(redeemJobStorageKey, JSON.stringify({ id: job.id }))
  } catch {
    // Ignore unavailable storage; polling still works for the current page lifetime.
  }
}

function readStoredRedeemJobId() {
  try {
    const raw = localStorage.getItem(redeemJobStorageKey)
    if (!raw) return null
    const parsed = JSON.parse(raw) as { id?: unknown }
    return typeof parsed.id === 'string' && parsed.id ? parsed.id : null
  } catch {
    return null
  }
}

function clearStoredRedeemJob() {
  try {
    localStorage.removeItem(redeemJobStorageKey)
  } catch {
    // Ignore unavailable storage.
  }
}

onMounted(() => {
  void resumeStoredRedeemJob()
})

onBeforeUnmount(clearRedeemPoll)
</script>
