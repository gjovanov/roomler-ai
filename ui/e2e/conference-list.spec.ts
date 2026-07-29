import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Room List', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Room List Org', `roomlist-${Date.now()}`)
    tenantId = tenant.id

    await loginViaUi(page, user.username, user.password)
  })

  test('rooms page shows created rooms', async ({ page }) => {
    // Create 2 rooms via API
    const room1 = await createRoomViaApi(token, tenantId, 'Weekly Standup')
    const room2 = await createRoomViaApi(token, tenantId, 'Design Review')

    await page.goto(`/tenant/${tenantId}/rooms`)

    // Both rooms should appear in the main RoomList tree (sidebar
    // also renders room names → strict-mode trip without main scope).
    await expect(page.locator('main').getByText('Weekly Standup')).toBeVisible({ timeout: 10000 })
    await expect(page.locator('main').getByText('Design Review')).toBeVisible()
  })

  test('clicking a room navigates to room view', async ({ page }) => {
    const room = await createRoomViaApi(token, tenantId, 'Navigate Test')

    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.locator('main').getByText('Navigate Test')).toBeVisible({ timeout: 10000 })

    // Click the room item in main (sidebar also has it)
    await page.locator('main').getByText('Navigate Test').click()

    // Should navigate to room view
    await expect(page).toHaveURL(new RegExp(`/tenant/${tenantId}/room/${room.id}`), {
      timeout: 10000,
    })
  })

  test('empty state is shown when no rooms exist', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/rooms`)

    await expect(page.getByText(/no rooms/i)).toBeVisible({ timeout: 10000 })
  })

  test('create room button opens dialog', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/rooms`)

    // Click create button
    await page.getByRole('button', { name: /create room/i }).click()

    // Dialog should appear with name field
    await expect(page.getByText(/room name/i).first()).toBeVisible({ timeout: 5000 })
  })

  test('sidebar has Rooms link', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/rooms`)

    // The sidebar should have a Rooms nav item
    await expect(page.getByRole('link', { name: /rooms/i })).toBeVisible({ timeout: 10000 })
  })

  test('dashboard room card links to rooms page', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}`)

    // The dashboard's Rooms stat card is a router-link (<v-card :to>
    // rendering an <a href=".../rooms">) inside <main>. Target it by
    // its href and scope to main — a bare getByText('Rooms').first()
    // matches the nav-drawer "Rooms" leaf (DOM-order before main) since
    // the S4 IA pivot, which isn't the "dashboard room card" this test
    // is about and click-hangs.
    // The dashboard has more than one link to /rooms (the Rooms stat
    // card + a quick-action button); the stat card is first in DOM.
    const roomCard = page.locator(`main a[href$="/tenant/${tenantId}/rooms"]`).first()
    await expect(roomCard).toBeVisible({ timeout: 10000 })
    await expect(roomCard).toContainText('Rooms')

    await roomCard.click()

    await expect(page).toHaveURL(new RegExp(`/tenant/${tenantId}/rooms`), {
      timeout: 10000,
    })
  })
})
