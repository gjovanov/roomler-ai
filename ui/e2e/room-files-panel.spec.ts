import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  joinRoomViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Room Files Panel', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string
  let roomId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Files Panel Org', `fp-${Date.now()}`)
    tenantId = tenant.id

    const room = await createRoomViaApi(token, tenantId, 'general', true)
    roomId = room.id
    await joinRoomViaApi(token, tenantId, roomId)

    await loginViaUi(page, user.username, user.password)
  })

  test('Files button is visible in chat toolbar', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)
    // Look for a files/attachment button in the toolbar area
    const filesBtn = page.locator('[data-testid="files-btn"], button:has(.mdi-paperclip), button:has(.mdi-attachment), button:has(.mdi-file)')
    await expect(filesBtn.first()).toBeVisible({ timeout: 10000 })
  })

  test('Clicking Files button opens the files panel', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)
    const filesBtn = page.locator('[data-testid="files-btn"], button:has(.mdi-paperclip), button:has(.mdi-attachment), button:has(.mdi-file)')
    await filesBtn.first().click()
    // The file panel should now be visible with its search field
    await expect(page.locator('.file-panel, [data-testid="file-panel"]').first()).toBeVisible({ timeout: 5000 })
  })

  test('Files panel shows "No files" empty state', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)
    const filesBtn = page.locator('[data-testid="files-btn"], button:has(.mdi-paperclip), button:has(.mdi-attachment), button:has(.mdi-file)')
    await filesBtn.first().click()
    await expect(page.getByText(/no files/i)).toBeVisible({ timeout: 10000 })
  })

  test('Upload a file and verify it appears in the panel', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)
    const filesBtn = page.locator('[data-testid="files-btn"], button:has(.mdi-paperclip), button:has(.mdi-attachment), button:has(.mdi-file)')
    await filesBtn.first().click()
    await expect(page.locator('.file-panel, [data-testid="file-panel"]').first()).toBeVisible({ timeout: 5000 })

    // Use the hidden file input to upload
    const fileInput = page.locator('.file-panel input[type="file"], [data-testid="file-panel"] input[type="file"]')
    await fileInput.setInputFiles({
      name: 'test-upload.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('Hello from E2E test!'),
    })

    // Verify the uploaded file appears
    await expect(page.getByText('test-upload.txt')).toBeVisible({ timeout: 10000 })
  })

  test('Close button closes the panel', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/room/${roomId}`)
    const filesBtn = page.locator('[data-testid="files-btn"], button:has(.mdi-paperclip), button:has(.mdi-attachment), button:has(.mdi-file)')
    await filesBtn.first().click()
    await expect(page.locator('.file-panel, [data-testid="file-panel"]').first()).toBeVisible({ timeout: 5000 })

    // Close the panel - look for a close button
    const closeBtn = page.locator('[data-testid="close-files-panel"], button:has(.mdi-close)').first()
    if (await closeBtn.isVisible()) {
      await closeBtn.click()
    } else {
      // If no close button, click the files toggle button again
      await filesBtn.first().click()
    }

    // Panel should no longer be visible
    await expect(page.locator('.file-panel, [data-testid="file-panel"]')).not.toBeVisible({ timeout: 5000 })
  })
})
