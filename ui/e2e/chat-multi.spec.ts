import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createChannelViaApi,
  joinChannelViaApi,
  sendMessageViaApi,
  addTenantMemberViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Multi-Participant Chat (dedup)', () => {
  const users: Array<{ user: ReturnType<typeof uniqueUser>; token: string; userId: string }> = []
  let tenantId: string
  let channelId: string
  let ownerToken: string

  test.beforeEach(async () => {
    users.length = 0

    // Register 4 users
    for (let i = 0; i < 4; i++) {
      const user = uniqueUser()
      const result = await registerUserViaApi(user)
      users.push({ user, token: result.access_token, userId: result.user.id })
    }

    ownerToken = users[0].token

    // Create tenant with first user
    const tenant = await createTenantViaApi(ownerToken, 'Chat Dedup Org', `chat-dedup-${Date.now()}`)
    tenantId = tenant.id

    // Add other users to tenant
    for (let i = 1; i < users.length; i++) {
      await addTenantMemberViaApi(ownerToken, tenantId, users[i].userId)
    }

    // Create a channel
    const channel = await createChannelViaApi(ownerToken, tenantId, `general-${Date.now()}`)
    channelId = channel.id

    // All users join the channel
    for (const u of users) {
      await joinChannelViaApi(u.token, tenantId, channelId)
    }
  })

  test('all messages visible with no duplicates', async ({ page }) => {
    // Each user sends a unique message via API
    const expectedMessages: string[] = []
    for (let i = 0; i < users.length; i++) {
      const content = `Message from user ${i + 1} - ${Date.now()}`
      await sendMessageViaApi(users[i].token, tenantId, channelId, content)
      expectedMessages.push(content)
    }

    // Login as first user and navigate to chat
    await loginViaUi(page, users[0].user.username, users[0].user.password)
    await page.goto(`/tenant/${tenantId}/channel/${channelId}`)

    // Wait for messages to load
    await expect(page.getByText(expectedMessages[0])).toBeVisible({ timeout: 10000 })

    // Verify all 4 messages are visible
    for (const msg of expectedMessages) {
      await expect(page.getByText(msg)).toBeVisible()
    }

    // Verify no duplicates: each message text should appear exactly once
    for (const msg of expectedMessages) {
      const count = await page.getByText(msg).count()
      expect(count).toBe(1)
    }
  })

  test('sending a message does not produce a duplicate', async ({ page, context }) => {
    // Login as first user
    await loginViaUi(page, users[0].user.username, users[0].user.password)
    await page.goto(`/tenant/${tenantId}/channel/${channelId}`)

    // Wait for chat view to load
    const input = page.getByPlaceholder(/type a message/i)
    await expect(input).toBeVisible({ timeout: 10000 })

    // Send a message through the UI
    const uniqueMsg = `No-dup test ${Date.now()}`
    await input.fill(uniqueMsg)
    await input.press('Enter')

    // Wait for the message to appear
    await expect(page.getByText(uniqueMsg)).toBeVisible({ timeout: 10000 })

    // Give time for any WS duplicate to arrive
    await page.waitForTimeout(2000)

    // Should appear exactly once
    const count = await page.getByText(uniqueMsg).count()
    expect(count).toBe(1)
  })
})
