// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineConfig, devices } from '@playwright/test'

export default defineConfig({
  testDir: './e2e/video',
  outputDir: './e2e/video/output',
  fullyParallel: false,
  retries: 0,
  workers: 1,
  preserveOutput: 'always',
  reporter: [['list']],
  use: {
    baseURL: process.env.E2E_BASE_URL || 'http://localhost:5000',
    video: { mode: 'on', size: { width: 1280, height: 720 } },
    viewport: { width: 1280, height: 720 },
    launchOptions: {
      slowMo: 80,
      args: [
        '--use-fake-device-for-media-stream',
        '--use-fake-ui-for-media-stream',
      ],
    },
  },
  projects: [
    {
      name: 'chromium',
      // ⚠️ `channel: 'chrome'` uses the INSTALLED Chrome, not Playwright's
      // bundled Chromium — and for this spec that is load-bearing, not a
      // preference. The bundled build ships without proprietary codecs, so a
      // remote-desktop session that negotiates H.264/HEVC connects and then
      // never decodes a frame: the canvas stays blank and the scene times out
      // with no error anywhere that names the cause. Cost three takes to find.
      use: { ...devices['Desktop Chrome'], channel: 'chrome' },
    },
  ],
  webServer: process.env.E2E_BASE_URL
    ? undefined
    : {
        command: 'bun run dev',
        port: 5173,
        reuseExistingServer: true,
        timeout: 30_000,
      },
})
