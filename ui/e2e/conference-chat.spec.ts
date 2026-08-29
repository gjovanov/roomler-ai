import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  startCallViaApi,
  joinCallViaApi,
  sendMessageViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Room Call Chat', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string
  let roomId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Chat Org', `chat-${Date.now()}`)
    tenantId = tenant.id

    const room = await createRoomViaApi(token, tenantId, 'Chat Meeting')
    roomId = room.id

    await startCallViaApi(token, tenantId, roomId)
    await joinCallViaApi(token, tenantId, roomId)

    await loginViaUi(page, user.username, user.password)
  })

  test('room call chat panel toggles visibility', async ({ page, context }) => {
    await context.grantPermissions(['camera', 'microphone'])
    await page.goto(`/tenant/${tenantId}/room/${roomId}/call`)

    // Before joining, chat toggle should not be visible
    await expect(page.locator('[class*="mdi-message-text"]')).not.toBeVisible()

    // Join the call
    await page.getByRole('button', { name: /join/i }).click()

    // After joining, chat panel should be visible (auto-opens)
    // The panel IS its editor: assert on that, not on the word "Chat".
    // `getByText('Chat')` matched four elements — this fixture's own org and
    // room are named "Chat Org" and "Chat Meeting" — so the assertion died on
    // a strict-mode violation, which reads like a product failure and is not.
    await expect(page.locator('.tiptap').first()).toBeVisible({ timeout: 15000 })

    // Click the chat toggle to hide
    await page.locator('button:has(.mdi-message-text)').first().click()
    await expect(page.locator('.tiptap')).not.toBeVisible()

    // Click again to show
    await page.locator('button:has(.mdi-message-text-outline)').first().click()
    await expect(page.locator('.tiptap')).toBeVisible()
  })

  test('send and receive room call chat message', async ({ page, context }) => {
    await context.grantPermissions(['camera', 'microphone'])
    await page.goto(`/tenant/${tenantId}/room/${roomId}/call`)

    // Join the call
    await page.getByRole('button', { name: /join/i }).click()
    // The panel IS its editor: assert on that, not on the word "Chat".
    // `getByText('Chat')` matched four elements — this fixture's own org and
    // room are named "Chat Org" and "Chat Meeting" — so the assertion died on
    // a strict-mode violation, which reads like a product failure and is not.
    await expect(page.locator('.tiptap').first()).toBeVisible({ timeout: 15000 })

    // Type into the TipTap editor (unified chat uses MessageEditor with TipTap)
    const editor = page.locator('.tiptap').first()
    await expect(editor).toBeVisible({ timeout: 5000 })
    await editor.click()
    await editor.pressSequentially('Hello from E2E!')
    await editor.press('Enter')

    // Verify the message appears in the chat panel
    await expect(page.getByText('Hello from E2E!')).toBeVisible({ timeout: 5000 })
  })
})
