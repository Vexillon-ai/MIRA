// SPDX-License-Identifier: AGPL-3.0-or-later

import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import path from 'path'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  // Q1.2 — re-enabled so /mira-sw.js (the push service worker) and
  // /favicon.svg get copied into dist/ on `npm run build`. Previously
  // false which silently broke the favicon link in index.html too.
  publicDir: 'public',
  resolve: {
    alias: { '@': path.resolve(__dirname, './src') },
  },
  server: {
    port: 5173,
    proxy: {
      '/api': { target: 'http://localhost:8082', changeOrigin: true },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: false,
  },
})
