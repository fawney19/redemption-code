<template>
  <section class="admin-grid codes-grid">
    <div class="panel">
      <div class="panel-header">
        <div>
          <h2>生成兑换码</h2>
          <p>独占分配账号</p>
        </div>
      </div>
      <div class="panel-body grid">
        <input v-model="batchForm.name" class="input" placeholder="批次名称" />
        <input v-model.number="batchForm.total_count" class="input" type="number" min="1" max="5000" placeholder="兑换码数量" />
        <input v-model.number="batchForm.accounts_per_code" class="input" type="number" min="1" max="100" placeholder="每码账号数" />
        <input v-model="batchForm.plan_filter_text" class="input" placeholder="套餐筛选，可选：plus,team" />
        <input v-model="batchForm.expires_at_text" class="input" placeholder="过期时间，可选：2026-07-01T00:00:00+08:00" />
        <button class="button primary" :disabled="busy" @click="createBatch">
          <Plus :size="15" />生成
        </button>
        <Transition name="fade">
          <div v-if="generatedCodes" class="generated-codes-wrap">
            <div class="terminal-bar"><span></span><span></span><span></span></div>
            <pre class="result mono dark-result">{{ generatedCodes }}</pre>
          </div>
        </Transition>
      </div>
    </div>

    <div class="panel">
      <div class="panel-header">
        <div>
          <h2>兑换码批次</h2>
          <p>复制、查看兑换状态</p>
        </div>
        <button class="button" @click="loadBatches"><RefreshCw :size="15" />刷新</button>
      </div>
      <div class="panel-body">
        <div class="table-wrap">
          <table class="table">
            <thead>
              <tr>
                <th>批次</th>
                <th>数量</th>
                <th>每码</th>
                <th>状态</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="batch in batches" :key="batch.id">
                <td><strong>{{ batch.name }}</strong><div class="muted mono">{{ batch.id }}</div></td>
                <td>{{ batch.redeemed_count }} / {{ batch.total_count }}</td>
                <td>{{ batch.accounts_per_code }}</td>
                <td><span class="badge" :class="batch.status">{{ batch.status }}</span></td>
                <td><button class="button ghost" @click="loadCodes(batch.id)">查看</button></td>
              </tr>
              <tr v-if="!batches.length">
                <td colspan="5" class="empty-row">
                  <Ticket :size="20" />
                  <span>暂无批次</span>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <Transition name="fade">
          <pre v-if="batchCodesText !== '选择批次查看兑换码状态'" class="result mono batch-result">{{ batchCodesText }}</pre>
        </Transition>
      </div>
    </div>
  </section>
</template>

<script setup lang="ts">
import { Plus, RefreshCw, Ticket } from 'lucide-vue-next'
import { useAdmin, createBatch, loadBatches, loadCodes } from '../composables/useAdmin'

const { batches, batchForm, batchCodes, generatedCodes, busy, batchCodesText } = useAdmin()
</script>
