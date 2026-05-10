import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  joinRoomViaApi,
  addTenantMemberViaApi,
  fetchMembersViaApi,
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
    // Login as admin
    await page.goto('/login')
    await page.locator('input').first().fill(adminUser.email)
    await page.locator('input[type="password"]').fill(adminUser.password)
    await page.getByRole('button', { name: /login/i }).click()
    await expect(page).toHaveURL(/\/$/, { timeout: 10000 })

    // Navigate to the room
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)
    await page.waitForTimeout(1500)

    // Click into the message editor
    const editor = page.locator('.tiptap.ProseMirror')
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
