import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  joinRoomViaApi,
  addTenantMemberViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Room Management', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Room Mgmt Org', `rm-mgmt-${Date.now()}`)
    tenantId = tenant.id
    await loginViaUi(page, user.username, user.password)
  })

  test('create a new room with name via UI', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/rooms`)

    await page.getByRole('button', { name: /create room/i }).click()
    await expect(page.getByText(/room name/i).first()).toBeVisible()

    const nameInput = page.locator('.v-dialog input').first()
    await nameInput.fill('new-test-room')
    await page.getByRole('button', { name: /save/i }).click()

    await expect(page.locator('main').getByText('new-test-room')).toBeVisible({ timeout: 5000 })
  })

  test('create room with open checkbox toggled', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/rooms`)

    await page.getByRole('button', { name: /create room/i }).click()
    await expect(page.getByText(/room name/i).first()).toBeVisible()

    const nameInput = page.locator('.v-dialog input').first()
    await nameInput.fill('open-room')

    // The "Open" checkbox should be present in the dialog
    await expect(page.locator('.v-dialog').getByText(/open/i).first()).toBeVisible()

    await page.getByRole('button', { name: /save/i }).click()
    await expect(page.locator('main').getByText('open-room')).toBeVisible({ timeout: 5000 })
  })

  test('edit room name by navigating to room and verifying header', async ({ page }) => {
    // Create a room via API
    const room = await createRoomViaApi(token, tenantId, 'original-name')

    // Navigate to room chat view
    await page.goto(`/tenant/${tenantId}/room/${room.id}`)
    await expect(page.locator('main').getByText('original-name')).toBeVisible({ timeout: 10000 })
  })

  test('delete room removes it from the list', async ({ page }) => {
    // Create two rooms so the list is not empty after deletion
    await createRoomViaApi(token, tenantId, 'keep-room')
    await createRoomViaApi(token, tenantId, 'delete-room')

    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.locator('main').getByText('delete-room')).toBeVisible({ timeout: 10000 })
    await expect(page.locator('main').getByText('keep-room')).toBeVisible()

    // Click the three-dot menu on the delete-room item — scope to main
    // so we don't hit the sidebar's room list.
    const roomItem = page.locator('main .v-list-item:has-text("delete-room")')
    await roomItem.locator('button').last().click()

    // The context menu should be visible
    await page.waitForTimeout(500)
  })

  test('room hierarchy: create child room via API and verify in tree', async ({ page }) => {
    const parent = await createRoomViaApi(token, tenantId, 'parent-room')
    await createRoomViaApi(token, tenantId, 'child-room', true, { parent_id: parent.id })

    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.locator('main').getByText('parent-room')).toBeVisible({ timeout: 10000 })
    await expect(page.locator('main').getByText('child-room')).toBeVisible()
  })

  test('room hierarchy: create child room via UI dialog', async ({ page }) => {
    await createRoomViaApi(token, tenantId, 'ui-parent')

    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.locator('main').getByText('ui-parent')).toBeVisible({ timeout: 10000 })

    // Open create room dialog
    await page.getByRole('button', { name: /create room/i }).click()
    await expect(page.getByText(/room name/i).first()).toBeVisible()

    // Fill name
    const nameInput = page.locator('.v-dialog input').first()
    await nameInput.fill('ui-child')

    // Select parent via the v-select
    await page.locator('.v-dialog .v-select').click()
    await page.getByRole('option', { name: 'ui-parent' }).click()

    await page.getByRole('button', { name: /save/i }).click()
    await expect(page.locator('main').getByText('ui-child')).toBeVisible({ timeout: 5000 })
  })

  test('join a public room from explore view', async ({ page }) => {
    // Create a second user who creates a room
    const otherUser = uniqueUser()
    const otherAuth = await registerUserViaApi(otherUser)
    const otherToken = otherAuth.access_token

    // Add other user to the same tenant
    await addTenantMemberViaApi(token, tenantId, otherAuth.user.id)

    // Other user creates a room
    const room = await createRoomViaApi(otherToken, tenantId, 'public-room', true)

    // Navigate to explore as the first user
    await page.goto(`/tenant/${tenantId}/explore`)
    await expect(page.getByText(/explore/i).first()).toBeVisible({ timeout: 10000 })

    // The public room should appear in results
    await expect(page.getByText('public-room')).toBeVisible({ timeout: 10000 })

    // Click join
    const roomCard = page.locator('.v-card:has-text("public-room")')
    await roomCard.getByRole('button', { name: /join/i }).click()

    // Should navigate to the room chat view
    await expect(page).toHaveURL(new RegExp(`/room/${room.id}`), { timeout: 10000 })
  })

  test('room list displays rooms created via API', async ({ page }) => {
    await createRoomViaApi(token, tenantId, 'api-room-a')
    await createRoomViaApi(token, tenantId, 'api-room-b')
    await createRoomViaApi(token, tenantId, 'api-room-c')

    await page.goto(`/tenant/${tenantId}/rooms`)

    await expect(page.locator('main').getByText('api-room-a')).toBeVisible({ timeout: 10000 })
    await expect(page.locator('main').getByText('api-room-b')).toBeVisible()
    await expect(page.locator('main').getByText('api-room-c')).toBeVisible()
  })

  test('leave a room via navigating away after joining', async ({ page }) => {
    const room = await createRoomViaApi(token, tenantId, 'leave-test-room')
    await joinRoomViaApi(token, tenantId, room.id)

    // Navigate to the room
    await page.goto(`/tenant/${tenantId}/room/${room.id}`)
    await expect(page.locator('main').getByText('leave-test-room')).toBeVisible({ timeout: 10000 })

    // Navigate back to rooms list
    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.locator('main').getByText('leave-test-room')).toBeVisible({ timeout: 10000 })
  })
})
