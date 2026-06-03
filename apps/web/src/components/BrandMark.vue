<template>
  <div class="brand" :class="{ compact }">
    <div class="brand-mark" aria-hidden="true">
      <svg width="20" height="20" viewBox="0 0 20 20" fill="none">
        <path d="M10 2L10 18M2 10L18 10M4.34 4.34L15.66 15.66M15.66 4.34L4.34 15.66" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>
      </svg>
    </div>
    <div>
      <strong>AetherPool</strong>
      <span>{{ subtitle }}</span>
    </div>
  </div>
</template>

<script setup lang="ts">
defineProps<{
  subtitle?: string
  compact?: boolean
}>()
</script>
