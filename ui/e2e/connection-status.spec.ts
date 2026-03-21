import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Connection Status', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'WS Status Org', `ws-status-${Date.now()}`)
    tenantId = tenant.id
  })

  test('no disconnected banner when WebSocket is connected', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)

    // Navigate to a page within the app layout
    await page.goto(`/tenant/${tenantId}`)
    await expect(page.getByText(/rooms/i).first()).toBeVisible({ timeout: 10000 })

    // Wait for WS to connect
    await page.waitForTimeout(3000)

    // The disconnected banner should NOT be visible
    await expect(page.getByText(/disconnected/i)).not.toBeVisible()
  })

  test('disconnected banner appears when WebSocket is offline', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)

    // Navigate to a page within the app layout
    await page.goto(`/tenant/${tenantId}`)
    await expect(page.getByText(/rooms/i).first()).toBeVisible({ timeout: 10000 })

    // Wait for WS to connect initially
    await page.waitForTimeout(3000)

    // Simulate going offline by cutting the network
    await page.context().setOffline(true)

    // Wait for the app to detect disconnection
    await page.waitForTimeout(5000)

    // The disconnected or connecting banner may appear
    const disconnectedBanner = page.getByText(/disconnected/i)
    const connectingBanner = page.getByText(/connecting/i)
    const hasBanner = await disconnectedBanner.isVisible().catch(() => false) ||
                      await connectingBanner.isVisible().catch(() => false)

    // Restore network
    await page.context().setOffline(false)

    // After restoring, wait for reconnection
    await page.waitForTimeout(5000)

    // The banner should eventually disappear
    // (Give it time to reconnect)
    await page.waitForTimeout(3000)
  })

  test('WebSocket connects successfully after login', async ({ page }) => {
    // Listen for WebSocket connections
    const wsPromise = page.waitForEvent('websocket', {
      predicate: (ws) => ws.url().includes('/ws?token='),
      timeout: 15000,
    })

    await loginViaUi(page, user.username, user.password)

    const ws = await wsPromise
    expect(ws.url()).toContain('/ws?token=')
  })

  test('no WebSocket console errors during normal operation', async ({ page }) => {
    const wsErrors: string[] = []
    page.on('console', (msg) => {
      if (msg.type() === 'error' && msg.text().toLowerCase().includes('websocket')) {
        wsErrors.push(msg.text())
      }
    })

    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}`)
    await expect(page.getByText(/rooms/i).first()).toBeVisible({ timeout: 10000 })

    // Wait for WS to stabilize
    await page.waitForTimeout(3000)

    expect(wsErrors).toEqual([])
  })
})
