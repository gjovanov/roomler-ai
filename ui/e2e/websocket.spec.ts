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
      // The app dials `/ws`, optionally `?tid=<tenant>`. The token USED to be
      // in this URL and deliberately is not any more (#691) — a query string
      // reaches access logs and `Referer`. Match on the PATH; the assertions
      // below then lock the new property instead of the retired one.
      predicate: (ws) => new URL(ws.url()).pathname === '/ws',
      timeout: 15000,
    })

    await loginViaUi(page, user.username, user.password)

    const ws = await wsPromise
    expect(new URL(ws.url()).pathname).toBe('/ws')
    // Cookie-only sessions: no credential may appear in the URL.
    expect(ws.url()).not.toContain('token=')
    // ⚠️ And it must STAY open. A refused upgrade (403 for an untrusted Origin
    // on the cookie path) still fires the `websocket` event, so asserting only
    // that a socket was created would pass against a stack where realtime is
    // completely broken — which is exactly how this lane went quiet.
    await page.waitForTimeout(2000)
    expect(ws.isClosed(), 'the /ws upgrade was refused or dropped').toBe(false)
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
      // The app dials `/ws`, optionally `?tid=<tenant>`. The token USED to be
      // in this URL and deliberately is not any more (#691) — a query string
      // reaches access logs and `Referer`. Match on the PATH; the assertions
      // below then lock the new property instead of the retired one.
      predicate: (ws) => new URL(ws.url()).pathname === '/ws',
      timeout: 15000,
    })
    await loginViaUi(page, user.username, user.password)

    const ws = await wsPromise
    expect(new URL(ws.url()).pathname).toBe('/ws')
    expect(ws.url()).not.toContain('token=')

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
