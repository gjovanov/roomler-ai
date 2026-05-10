import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

const API_URL = process.env.E2E_API_URL || 'http://localhost:5001'

test.describe('Chat', () => {
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

    // Create a room via API
    const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/room`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({ name: 'general', is_open: true }),
    })
    const room = (await resp.json()) as { id: string }
    roomId = room.id

    // Join room
    await fetch(`${API_URL}/api/tenant/${tenantId}/room/${roomId}/join`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${token}` },
    })

    await loginViaUi(page, user.username, user.password)
  })

  test('chat view loads with empty state', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)
    await expect(page.getByText(/no messages/i)).toBeVisible({ timeout: 10000 })
  })

  test('message input is visible and functional', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)
    // The message editor is a TipTap contenteditable div, not a plain
    // input/textarea — getByPlaceholder doesn't match TipTap's
    // data-placeholder attribute. Target the ProseMirror root instead.
    const editor = page.locator('.ProseMirror[contenteditable="true"]').first()
    await expect(editor).toBeVisible({ timeout: 10000 })
    await editor.click()
    await page.keyboard.type('Hello from E2E!')
    await expect(editor).toContainText('Hello from E2E!')
  })
})
