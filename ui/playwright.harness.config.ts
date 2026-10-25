// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #2888 — the soft-keyboard harness (`e2e-harness/`): the viewer's key path in
// a real Chromium, against a one-page harness the Vite DEV server serves (the
// page imports the real `MobileKeyboard.vue` and the real key-routing code;
// `vite build` never includes it). Run it with
//   bunx playwright test -c playwright.harness.config.ts
// (on a Windows box with bun, `npx playwright test -c …` — bun trips over a
// `.ts` config, see playwright.config.cjs). Needs no backend, no database;
// CI runs it on any PR that touches `ui/` (`ui-phone-smoke.yml`).
import { defineConfig, devices } from '@playwright/test'

const PORT = 4174

export default defineConfig({
  testDir: './e2e-harness',
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI
    ? [['list'], ['html', { open: 'never', outputFolder: 'playwright-report-harness' }]]
    : 'list',
  outputDir: 'test-results-harness',
  use: {
    baseURL: `http://localhost:${PORT}`,
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
    // The paste path reads what `navigator.clipboard.writeText` put there.
    permissions: ['clipboard-read', 'clipboard-write'],
  },
  projects: [
    // A tablet with a hardware keyboard is the field case; the key events are
    // the same on every form factor, the touch flag only keeps `pointerInside`
    // honest about what a finger does (it leaves when it lifts).
    { name: 'chromium-touch', use: { ...devices['Desktop Chrome'], hasTouch: true } },
  ],
  webServer: {
    command: `bunx vite --port ${PORT} --strictPort`,
    port: PORT,
    reuseExistingServer: !process.env.CI,
    timeout: 90_000,
  },
})
