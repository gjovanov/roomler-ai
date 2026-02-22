import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Rooms', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Test Org', `test-${Date.now()}`)
    tenantId = tenant.id
    await loginViaUi(page, user.username, user.password)
  })

  test('room list page loads', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.getByText(/rooms/i).first()).toBeVisible()
  })

  test('create room dialog opens and closes', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/rooms`)
    await page.getByRole('button', { name: /create room/i }).click()
    // Dialog should be visible
    await expect(page.getByText(/room name/i).first()).toBeVisible()

    // Cancel
    await page.getByRole('button', { name: /cancel/i }).click()
  })

  test('create room via UI and see it in room list', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/rooms`)

    // Open create dialog
    await page.getByRole('button', { name: /create room/i }).click()
    await expect(page.getByText(/room name/i).first()).toBeVisible()

    // Fill in room name and save
    const nameInput = page.locator('.v-dialog input').first()
    await nameInput.fill('my-test-room')
    await page.getByRole('button', { name: /save/i }).click()

    // Verify the new room appears in the list
    await expect(page.getByText('my-test-room')).toBeVisible({ timeout: 5000 })
  })

  test('create duplicate room shows error alert', async ({ page }) => {
    // Pre-create a room via API so we have a duplicate name to test against
    await createRoomViaApi(token, tenantId, 'duplicate-rm')

    await page.goto(`/tenant/${tenantId}/rooms`)

    // Open create dialog
    await page.getByRole('button', { name: /create room/i }).click()
    await expect(page.getByText(/room name/i).first()).toBeVisible()

    // Try to create a room with the same name
    const nameInput = page.locator('.v-dialog input').first()
    await nameInput.fill('duplicate-rm')
    await page.getByRole('button', { name: /save/i }).click()

    // The error alert should be displayed inside the dialog
    await expect(page.locator('.v-dialog .v-alert')).toBeVisible({ timeout: 5000 })
  })

  test('room list displays rooms created via API', async ({ page }) => {
    // Create rooms via API
    await createRoomViaApi(token, tenantId, 'api-room-1')
    await createRoomViaApi(token, tenantId, 'api-room-2')

    await page.goto(`/tenant/${tenantId}/rooms`)

    // Both rooms should appear in the list (validates store parses plain array)
    await expect(page.getByText('api-room-1')).toBeVisible({ timeout: 10000 })
    await expect(page.getByText('api-room-2')).toBeVisible()
  })

  test('explore page loads with search', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/explore`)
    await expect(page.getByText(/explore/i).first()).toBeVisible()
    const searchInput = page.locator('input').first()
    await expect(searchInput).toBeVisible()
  })
})
