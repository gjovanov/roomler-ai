import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  startCallViaApi,
  endCallViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

// ── Fix 1: Dashboard "Start Call" creates unique room with media_settings ──

test.describe('Fix 1: Dashboard Start Call', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Fix1 Org', `fix1-${Date.now()}`)
    tenantId = tenant.id
    await loginViaUi(page, user.username, user.password)
  })

  test('start call creates room and navigates to call view', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}`)
    await expect(page.getByRole('button', { name: /start call/i })).toBeVisible({ timeout: 10000 })

    await page.getByRole('button', { name: /start call/i }).click()

    // Should navigate to room call view
    await expect(page).toHaveURL(/\/room\/[a-f0-9]+\/call/, { timeout: 15000 })
  })

  test('room created with media_settings via API has has_media=true', async () => {
    const room = await createRoomViaApi(token, tenantId, 'media-test', true, {
      media_settings: {},
    })
    expect(room.has_media).toBe(true)
  })

  test('room created without media_settings has has_media=false', async () => {
    const room = await createRoomViaApi(token, tenantId, 'no-media-test')
    expect(room.has_media).toBe(false)
  })
})

// ── Fix 2: Call button in room chat view ──

test.describe('Fix 2: Chat View Call Button', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Fix2 Org', `fix2-${Date.now()}`)
    tenantId = tenant.id
    await loginViaUi(page, user.username, user.password)
  })

  test('chat view shows Start Call button for media-enabled room', async ({ page }) => {
    const room = await createRoomViaApi(token, tenantId, 'media-chat-room', true, {
      media_settings: {},
    })

    await page.goto(`/tenant/${tenantId}/room/${room.id}`)
    await expect(page.getByRole('button', { name: /start call/i })).toBeVisible({ timeout: 10000 })
  })

  test('chat view does NOT show call button for non-media room', async ({ page }) => {
    const room = await createRoomViaApi(token, tenantId, 'text-only-room')

    await page.goto(`/tenant/${tenantId}/room/${room.id}`)
    // Wait for the room header to load
    await expect(page.getByText('text-only-room')).toBeVisible({ timeout: 10000 })
    // Call button should not exist
    await expect(page.getByRole('button', { name: /start call/i })).not.toBeVisible()
  })

  test('clicking Start Call in chat navigates to call view', async ({ page }) => {
    const room = await createRoomViaApi(token, tenantId, 'navigate-call-room', true, {
      media_settings: {},
    })

    await page.goto(`/tenant/${tenantId}/room/${room.id}`)
    await expect(page.getByRole('button', { name: /start call/i })).toBeVisible({ timeout: 10000 })

    await page.getByRole('button', { name: /start call/i }).click()
    await expect(page).toHaveURL(new RegExp(`/room/${room.id}/call`), { timeout: 10000 })
  })

  test('chat view shows Join Call when call is active', async ({ page }) => {
    const room = await createRoomViaApi(token, tenantId, 'active-call-room', true, {
      media_settings: {},
    })

    // Start a call via API so conference_status is InProgress
    await startCallViaApi(token, tenantId, room.id)

    await page.goto(`/tenant/${tenantId}/room/${room.id}`)
    // Should show "Join Call" since conference is in progress
    await expect(page.getByRole('button', { name: /join call/i })).toBeVisible({ timeout: 10000 })
  })
})

// ── Fix 3: Child room creation UI ──

test.describe('Fix 3: Child Room Creation', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Fix3 Org', `fix3-${Date.now()}`)
    tenantId = tenant.id
    await loginViaUi(page, user.username, user.password)
  })

  test('create room dialog has parent room selector', async ({ page }) => {
    await createRoomViaApi(token, tenantId, 'parent-room')

    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.getByText('parent-room')).toBeVisible({ timeout: 10000 })

    // Open create dialog
    await page.getByRole('button', { name: /create room/i }).click()
    await expect(page.getByText(/room name/i).first()).toBeVisible()

    // Parent selector should be visible — check the combobox element
    await expect(page.locator('.v-dialog .v-select')).toBeVisible()
  })

  test('create child room via parent selector in dialog', async ({ page }) => {
    await createRoomViaApi(token, tenantId, 'parent-for-child')

    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.getByText('parent-for-child')).toBeVisible({ timeout: 10000 })

    // Open create dialog
    await page.getByRole('button', { name: /create room/i }).click()
    await expect(page.getByText(/room name/i).first()).toBeVisible()

    // Fill in child name
    const nameInput = page.locator('.v-dialog input').first()
    await nameInput.fill('child-room')

    // Select parent via the v-select
    await page.locator('.v-dialog .v-select').click()
    await page.getByRole('option', { name: 'parent-for-child' }).click()

    // Save
    await page.getByRole('button', { name: /save/i }).click()

    // Child room should appear in the list
    await expect(page.getByText('child-room')).toBeVisible({ timeout: 5000 })
  })

  test('room tree item has context menu with Create Sub-Room', async ({ page }) => {
    await createRoomViaApi(token, tenantId, 'context-menu-room')

    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.getByText('context-menu-room')).toBeVisible({ timeout: 10000 })

    // Click the three-dot menu button (last button in the room's list item)
    const roomItem = page.locator('.v-list-item:has-text("context-menu-room")')
    await roomItem.locator('button').last().click()

    // Menu should show "Create Sub-Room"
    await expect(page.getByText(/create sub-room/i)).toBeVisible({ timeout: 3000 })
  })

  test('child room created via API shows in tree', async ({ page }) => {
    const parent = await createRoomViaApi(token, tenantId, 'tree-parent')
    await createRoomViaApi(token, tenantId, 'tree-child', true, { parent_id: parent.id })

    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.getByText('tree-parent')).toBeVisible({ timeout: 10000 })
    await expect(page.getByText('tree-child')).toBeVisible()
  })
})

// ── Fix 4: Call start/end notifications ──

test.describe('Fix 4: Call Notifications', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Fix4 Org', `fix4-${Date.now()}`)
    tenantId = tenant.id
    await loginViaUi(page, user.username, user.password)
  })

  test('room tree shows green indicator when call is active', async ({ page }) => {
    const room = await createRoomViaApi(token, tenantId, 'indicator-room', true, {
      media_settings: {},
    })

    // Start a call via API so conference_status = InProgress in DB
    await startCallViaApi(token, tenantId, room.id)

    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.getByText('indicator-room')).toBeVisible({ timeout: 10000 })

    // The room item should have a badge (role="status" rendered by Vuetify v-badge)
    const roomRow = page.locator('.v-list-item:has-text("indicator-room")')
    await expect(roomRow.getByRole('status')).toBeVisible({ timeout: 5000 })
  })

  test('room tree hides indicator after call ends', async ({ page }) => {
    const room = await createRoomViaApi(token, tenantId, 'end-indicator-room', true, {
      media_settings: {},
    })

    // Start and then end a call
    await startCallViaApi(token, tenantId, room.id)
    await endCallViaApi(token, tenantId, room.id)

    await page.goto(`/tenant/${tenantId}/rooms`)
    await expect(page.getByText('end-indicator-room')).toBeVisible({ timeout: 10000 })

    // Badge should not be visible since call ended
    const roomRow = page.locator('.v-list-item:has-text("end-indicator-room")')
    await expect(roomRow.getByRole('status')).not.toBeVisible()
  })

  test('call notification snackbar appears when call starts via WS', async ({ page }) => {
    const room = await createRoomViaApi(token, tenantId, 'notify-room', true, {
      media_settings: {},
    })

    // Navigate to a page with AppLayout (dashboard)
    await page.goto(`/tenant/${tenantId}`)
    await expect(page.getByText(/rooms/i).first()).toBeVisible({ timeout: 10000 })

    // Wait for WS to connect (the app auto-connects on mount)
    await page.waitForTimeout(2000)

    // Start a call via API — this triggers room:call_started WS event
    await startCallViaApi(token, tenantId, room.id)

    // Snackbar should appear with room name
    await expect(page.getByText(/call started in notify-room/i)).toBeVisible({ timeout: 10000 })

    // Join button should be in the snackbar
    await expect(page.locator('.v-snackbar').getByRole('button', { name: /join/i })).toBeVisible()
  })

  test('clicking Join in snackbar navigates to call view', async ({ page }) => {
    const room = await createRoomViaApi(token, tenantId, 'join-snackbar-room', true, {
      media_settings: {},
    })

    await page.goto(`/tenant/${tenantId}`)
    await expect(page.getByText(/rooms/i).first()).toBeVisible({ timeout: 10000 })
    await page.waitForTimeout(3000)

    await startCallViaApi(token, tenantId, room.id)
    await expect(page.getByText(/call started in join-snackbar-room/i)).toBeVisible({ timeout: 10000 })

    // Click Join
    await page.locator('.v-snackbar').getByRole('button', { name: /join/i }).click()

    // Should navigate to call view
    await expect(page).toHaveURL(new RegExp(`/room/${room.id}/call`), { timeout: 10000 })
  })
})
