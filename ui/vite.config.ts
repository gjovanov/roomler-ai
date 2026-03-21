import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import vuetify from 'vite-plugin-vuetify'
import { resolve } from 'path'

const apiTarget = process.env.VITE_API_URL || 'http://localhost:5001'

export default defineConfig({
  plugins: [
    vue(),
    vuetify({ autoImport: true }),
  ],
  resolve: {
    alias: {
      '@': resolve(__dirname, 'src'),
    },
  },
  build: {
    rollupOptions: {
      output: {
        manualChunks: {
          vuetify: ['vuetify'],
          tiptap: ['@tiptap/starter-kit', '@tiptap/vue-3'],
          mediasoup: ['mediasoup-client'],
        },
      },
    },
  },
  server: {
    port: 5000,
    proxy: {
      '/api': {
        target: apiTarget,
        changeOrigin: true,
      },
      '/ws': {
        target: apiTarget,
        changeOrigin: true,
        ws: true,
        rewriteWsOrigin: true,
      },
    },
  },
})
