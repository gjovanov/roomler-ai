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
      use: { ...devices['Desktop Chrome'] },
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
