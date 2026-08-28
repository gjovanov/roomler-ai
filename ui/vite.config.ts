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
        // FR-25 — function form, NOT the object form it replaced.
        //
        // The object form listed only `@tiptap/starter-kit` + `@tiptap/vue-3`,
        // so the other seven tiptap packages (extension-mention, suggestion,
        // tiptap-markdown, …) were chunked wherever they were imported from —
        // the message-editor chunk. Both chunks then carried their OWN copy of
        // `prosemirror-model`, and a node built by one instance is rejected by
        // the other: picking a mention threw
        //   "Can not convert <mention, ' '> to a Fragment
        //    (looks like multiple versions of prosemirror-model were loaded)"
        // in EVERY editor (room chat and the in-call chat alike).
        //
        // Matching the whole family by path keeps prosemirror single-instance
        // no matter which tiptap package a future import pulls in. ⚠️ Verify
        // with: grep -c "multiple versions of prosemirror-model" dist/assets/*.js
        // — it must appear in exactly ONE chunk.
        manualChunks(id: string) {
          if (!id.includes('node_modules')) return
          if (/[\\/]node_modules[\\/](@tiptap[\\/]|prosemirror-|tiptap-markdown|y-prosemirror)/.test(id)) {
            return 'tiptap'
          }
          if (/[\\/]node_modules[\\/]vuetify[\\/]/.test(id)) return 'vuetify'
          if (/[\\/]node_modules[\\/]mediasoup-client[\\/]/.test(id)) return 'mediasoup'
          if (/[\\/]node_modules[\\/]d3-(array|scale|shape|time|time-format)[\\/]/.test(id)) {
            return 'charts'
          }
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
