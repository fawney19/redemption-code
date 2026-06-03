<template>
  <Teleport to="body">
    <TransitionGroup name="toast" tag="div" class="toast-container">
      <div v-for="toast in toasts" :key="toast.id" class="toast-item" :class="toast.type">
        <CircleCheck v-if="toast.type === 'success'" />
        <CircleAlert v-else-if="toast.type === 'error'" />
        <Info v-else />
        <span>{{ toast.message }}</span>
      </div>
    </TransitionGroup>
  </Teleport>
</template>

<script setup lang="ts">
import { CircleAlert, CircleCheck, Info } from 'lucide-vue-next'
import { useToast } from '../composables/useToast'

const { toasts } = useToast()
</script>
