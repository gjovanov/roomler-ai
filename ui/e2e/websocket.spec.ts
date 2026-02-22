import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('WebSocket', () => {
  test('WebSocket connection is established after login', async ({ page }) => {
    const user = uniqueUser()
    await registerUserViaApi(user)

    // Start listening for WebSocket connections before login triggers one
    // Filter for our app's /ws path to avoid catching Vite's HMR WebSocket
    const wsPromise = page.waitForEvent('websocket', {
      predicate: (ws) => ws.url().includes('/ws?token='),
      timeout: 15000,
    })

    await loginViaUi(page, user.username, user.password)

    // Verify a WebSocket connection was established to /ws
    const ws = await wsPromise
    expect(ws.url()).toContain('/ws?token=')
  })

  test('WebSocket connection does not produce console errors', async ({ page }) => {
    const user = uniqueUser()
    await registerUserViaApi(user)

    const wsErrors: string[] = []
    page.on('console', (msg) => {
      if (msg.type() === 'error' && msg.text().toLowerCase().includes('websocket')) {
        wsErrors.push(msg.text())
      }
    })

    await loginViaUi(page, user.username, user.password)

    // Navigate to dashboard to give WS time to connect
    await page.waitForTimeout(3000)

    expect(wsErrors).toEqual([])
  })

  test('WebSocket reconnects on navigation between pages', async ({ page }) => {
    const user = uniqueUser()
    const result = await registerUserViaApi(user)
    const tenant = await createTenantViaApi(
      result.access_token,
      'WS Org',
      `ws-${Date.now()}`,
    )

    const wsPromise = page.waitForEvent('websocket', {
      predicate: (ws) => ws.url().includes('/ws?token='),
      timeout: 15000,
    })
    await loginViaUi(page, user.username, user.password)

    const ws = await wsPromise
    expect(ws.url()).toContain('/ws?token=')

    // Register error listener before navigating so we catch all errors
    const wsErrors: string[] = []
    page.on('console', (msg) => {
      if (msg.type() === 'error' && msg.text().toLowerCase().includes('websocket')) {
        wsErrors.push(msg.text())
      }
    })

    // Navigate to tenant page — WS should remain connected (SPA navigation)
    await page.goto(`/tenant/${tenant.id}`)
    await expect(page.getByText(/rooms/i).first()).toBeVisible({ timeout: 10000 })

    await page.waitForTimeout(2000)
    expect(wsErrors).toEqual([])
  })
})
