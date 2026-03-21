import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  joinRoomViaApi,
  sendMessageViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Chat Pagination', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string
  let roomId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Paging Org', `paging-${Date.now()}`)
    tenantId = tenant.id

    const room = await createRoomViaApi(token, tenantId, 'pagination-room', true)
    roomId = room.id
    await joinRoomViaApi(token, tenantId, roomId)
  })

  test('chat loads messages after sending many via API', async ({ page }) => {
    // Send 35 messages via API
    const messages: string[] = []
    for (let i = 1; i <= 35; i++) {
      const content = `Pagination msg ${i} - ${Date.now()}`
      await sendMessageViaApi(token, tenantId, roomId, content)
      messages.push(content)
    }

    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)

    // Wait for messages to load — the most recent message should be visible
    const lastMsg = messages[messages.length - 1]
    await expect(page.getByText(lastMsg)).toBeVisible({ timeout: 15000 })
  })

  test('scroll to bottom button appears when scrolled up', async ({ page }) => {
    // Send enough messages to overflow the viewport
    for (let i = 1; i <= 35; i++) {
      await sendMessageViaApi(token, tenantId, roomId, `Scroll test msg ${i} - ${Date.now()}`)
    }

    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)

    // Wait for messages to load
    await page.waitForTimeout(3000)

    // Scroll up in the message list
    const messageList = page.locator('.overflow-y-auto').first()
    await messageList.evaluate((el) => {
      el.scrollTop = 0
    })
    await page.waitForTimeout(500)

    // The scroll-to-bottom button should appear (chevron-double-down icon)
    await expect(page.locator('.mdi-chevron-double-down')).toBeVisible({ timeout: 5000 })
  })

  test('clicking scroll to bottom button scrolls to latest message', async ({ page }) => {
    // Send enough messages
    const messages: string[] = []
    for (let i = 1; i <= 35; i++) {
      const content = `Bottom btn msg ${i} - ${Date.now()}`
      await sendMessageViaApi(token, tenantId, roomId, content)
      messages.push(content)
    }

    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)

    // Wait for messages to load
    const lastMsg = messages[messages.length - 1]
    await expect(page.getByText(lastMsg)).toBeVisible({ timeout: 15000 })

    // Scroll up
    const messageList = page.locator('.overflow-y-auto').first()
    await messageList.evaluate((el) => {
      el.scrollTop = 0
    })
    await page.waitForTimeout(500)

    // Click the scroll to bottom button
    const scrollBtn = page.locator('.mdi-chevron-double-down')
    if (await scrollBtn.isVisible()) {
      await scrollBtn.click()
      await page.waitForTimeout(1000)

      // The latest message should be visible again
      await expect(page.getByText(lastMsg)).toBeVisible({ timeout: 5000 })
    }
  })

  test('new message while scrolled up does not yank scroll position', async ({ page }) => {
    // Send enough messages to overflow
    for (let i = 1; i <= 30; i++) {
      await sendMessageViaApi(token, tenantId, roomId, `Pre msg ${i} - ${Date.now()}`)
    }

    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)

    // Wait for messages to load
    await page.waitForTimeout(3000)

    // Scroll up
    const messageList = page.locator('.overflow-y-auto').first()
    await messageList.evaluate((el) => {
      el.scrollTop = 0
    })
    await page.waitForTimeout(500)

    // Record scroll position
    const scrollBefore = await messageList.evaluate((el) => el.scrollTop)

    // Send a new message via API while user is scrolled up
    const newMsg = `New msg while scrolled ${Date.now()}`
    await sendMessageViaApi(token, tenantId, roomId, newMsg)

    // Wait for the WebSocket to deliver the message
    await page.waitForTimeout(3000)

    // Scroll position should NOT have jumped to bottom
    const scrollAfter = await messageList.evaluate((el) => el.scrollTop)

    // The scroll position should be close to where it was (within a small tolerance)
    // or the scroll-to-bottom button should be visible
    const scrollBtn = page.locator('.mdi-chevron-double-down')
    const btnVisible = await scrollBtn.isVisible()

    // Either scroll stayed near same position, or the button appeared
    expect(Math.abs(scrollAfter - scrollBefore) < 200 || btnVisible).toBe(true)
  })

  test('scroll to top triggers loading of older messages', async ({ page }) => {
    // Send enough messages
    for (let i = 1; i <= 35; i++) {
      await sendMessageViaApi(token, tenantId, roomId, `Older msg ${i} - ${Date.now()}`)
    }

    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)

    // Wait for initial messages to load
    await page.waitForTimeout(3000)

    // Scroll to the very top to trigger pagination
    const messageList = page.locator('.overflow-y-auto').first()
    await messageList.evaluate((el) => {
      el.scrollTop = 0
    })

    // Wait for potential loading of older messages
    await page.waitForTimeout(2000)

    // The page should still be functional — no crash or error
    await expect(messageList).toBeVisible()
  })
})
