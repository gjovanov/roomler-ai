import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  joinRoomViaApi,
  addTenantMemberViaApi,
  fetchMembersViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Mentions', () => {
  let adminToken: string
  let memberToken: string
  let tenantId: string
  let roomId: string
  let adminUser: ReturnType<typeof uniqueUser>
  let memberUser: ReturnType<typeof uniqueUser>
  let memberUserId: string

  test.beforeAll(async () => {
    // Setup: admin creates tenant + room, member joins
    adminUser = uniqueUser()
    memberUser = uniqueUser()

    const adminAuth = await registerUserViaApi(adminUser)
    adminToken = adminAuth.access_token

    const tenant = await createTenantViaApi(adminToken, 'Mention Corp', `mention_${Date.now()}`)
    tenantId = tenant.id

    const room = await createRoomViaApi(adminToken, tenantId, 'mention-test', true)
    roomId = room.id

    // Admin (creator) is auto-joined as a member.
    const memberAuth = await registerUserViaApi(memberUser)
    memberToken = memberAuth.access_token
    memberUserId = memberAuth.user.id

    await addTenantMemberViaApi(adminToken, tenantId, memberUserId)
    await joinRoomViaApi(memberToken, tenantId, roomId)
  })

  test('room members API returns paginated items with user details', async () => {
    const data = await fetchMembersViaApi(adminToken, tenantId, roomId)

    expect(data.items).toBeDefined()
    expect(Array.isArray(data.items)).toBe(true)
    expect(data.total).toBeGreaterThanOrEqual(2)

    // Every member should have enriched user details
    for (const member of data.items) {
      expect(member.id).toBeTruthy()
      expect(member.user_id).toBeTruthy()
      expect(member.display_name).toBeTruthy()
      expect(member.username).toBeTruthy()
    }

    // Both admin and member should be present
    const usernames = data.items.map((m) => m.username)
    expect(usernames).toContain(adminUser.username)
    expect(usernames).toContain(memberUser.username)
  })

  test('mention autocomplete shows room members in chat', async ({ page }) => {
    // Use loginViaUi (same path that chat.spec.ts:50 + chat-multi
    // .spec.ts:78 use to land on the chat view reliably). The
    // previous inline form-fill had a permissive URL regex that
    // would pass even if login silently failed and left the page at
    // /login, then the subsequent /tenant/.../room goto would
    // redirect back to /landing and the editor would never mount.
    await loginViaUi(page, adminUser.username, adminUser.password)

    // Navigate to the room + wait for network to go idle so all
    // lazy-loaded chunks (chat view, TipTap editor) have a chance to
    // hydrate before we target the contenteditable. The 1500ms hard
    // wait was racy — first-attempt could find the SPA still loading.
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)
    await page.waitForLoadState('networkidle', { timeout: 30000 })

    // Click into the message editor (the contenteditable child of the
    // TipTap wrapper). Wait explicitly for it to be visible so click
    // doesn't fall into a 30s hard timeout from the implicit retries.
    const editor = page.locator('.ProseMirror[contenteditable="true"]').first()
    await expect(editor).toBeVisible({ timeout: 30000 })
    await editor.click()

    // Type @ to trigger mention autocomplete
    await editor.pressSequentially('@')
    await page.waitForTimeout(500)

    // The mention dropdown should appear with room members
    const mentionList = page.locator('.mention-list')
    await expect(mentionList).toBeVisible({ timeout: 3000 })

    // Should show at least the member user (and special mentions like @everyone, @here)
    const mentionItems = mentionList.locator('.mention-item')
    const count = await mentionItems.count()
    expect(count).toBeGreaterThanOrEqual(3) // @everyone + @here + at least 1 member

    // Verify the member user appears by typing part of their name
    await editor.pressSequentially(memberUser.displayName.split(' ')[0])
    await page.waitForTimeout(500)

    // Should filter to show the member
    const filteredItems = mentionList.locator('.mention-item')
    const filteredCount = await filteredItems.count()
    expect(filteredCount).toBeGreaterThanOrEqual(1)

    // The mention name should contain the member's display name
    const mentionText = await filteredItems.first().locator('.mention-name').textContent()
    expect(mentionText?.toLowerCase()).toContain(
      memberUser.displayName.split(' ')[0].toLowerCase(),
    )
  })
})
