import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

export default defineConfig({
  plugins: [vue()],
  server: {
    port: 5178,
    proxy: {
      '/api': {
        target: process.env.VITE_API_BASE || 'http://127.0.0.1:8318',
        changeOrigin: true,
      },
      '/health': {
        target: process.env.VITE_API_BASE || 'http://127.0.0.1:8318',
        changeOrigin: true,
      },
    },
  },
})
