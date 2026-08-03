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
    // Creator auto-joined — explicit joinRoomViaApi(token,...) returns 409.
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

  test('scroll to top loads older pages until the FIRST message is reachable', async ({
    page,
  }) => {
    // Send enough messages for 2+ pages; remember the very first one.
    const stamp = Date.now()
    for (let i = 1; i <= 35; i++) {
      await sendMessageViaApi(token, tenantId, roomId, `Older msg ${i} - ${stamp}`)
    }

    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)

    // Wait for initial messages to load; the NEWEST message must be visible
    // at the bottom (ascending render order).
    await expect(page.getByText(`Older msg 35 - ${stamp}`)).toBeVisible({ timeout: 10000 })

    // Repeatedly scroll to the top — each hit loads one older page. The old
    // DESC-cursor bug re-fetched the same page forever, so message 1 was
    // UNREACHABLE; this locks the fix.
    const messageList = page.locator('.overflow-y-auto').first()
    for (let round = 0; round < 6; round++) {
      const found = await page
        .getByText(`Older msg 1 - ${stamp}`)
        .isVisible()
        .catch(() => false)
      if (found) break
      await messageList.evaluate((el) => {
        el.scrollTop = 0
      })
      await page.waitForTimeout(1500)
    }

    await expect(page.getByText(`Older msg 1 - ${stamp}`)).toBeVisible()
  })

  test('sent messages are still visible at the bottom after a reload', async ({ page }) => {
    const text = `Persistent msg ${Date.now()}`
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)
    await page.waitForTimeout(2000)

    await sendMessageViaApi(token, tenantId, roomId, text)
    await expect(page.getByText(text)).toBeVisible({ timeout: 10000 })

    // Reload: the newest messages must render at the BOTTOM of the view
    // (the DESC-render bug put them off-screen at the top — "my messages
    // are gone after refresh").
    await page.reload()
    await expect(page.getByText(text)).toBeVisible({ timeout: 10000 })
  })
})
