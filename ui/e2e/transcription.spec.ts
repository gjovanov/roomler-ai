import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  startCallViaApi,
  joinCallViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Transcription UI', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string
  let roomId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Transcribe Org', `transcribe-${Date.now()}`)
    tenantId = tenant.id

    const room = await createRoomViaApi(token, tenantId, 'Transcription Meeting')
    roomId = room.id

    await startCallViaApi(token, tenantId, roomId)
    await joinCallViaApi(token, tenantId, roomId)

    await loginViaUi(page, user.username, user.password)
  })

  test('transcription menu opens with model options', async ({ page, context }) => {
    await context.grantPermissions(['camera', 'microphone'])
    await page.goto(`/tenant/${tenantId}/room/${roomId}/call`)

    // Join the call
    await page.getByRole('button', { name: /join/i }).click()
    await expect(page.getByText('Chat')).toBeVisible({ timeout: 15000 })

    // Click the subtitles button to open the transcription menu
    await page.locator('button:has(.mdi-subtitles-outline)').first().click()

    // Verify model options are visible
    await expect(page.getByText('Transcription Model')).toBeVisible()
    await expect(page.getByText('Whisper')).toBeVisible()
    await expect(page.getByText('Canary')).toBeVisible()
  })

  test('model selection persists across menu open/close', async ({ page, context }) => {
    await context.grantPermissions(['camera', 'microphone'])
    await page.goto(`/tenant/${tenantId}/room/${roomId}/call`)

    await page.getByRole('button', { name: /join/i }).click()
    await expect(page.getByText('Chat')).toBeVisible({ timeout: 15000 })

    // Open menu and select Canary
    await page.locator('button:has(.mdi-subtitles-outline)').first().click()
    await expect(page.getByText('Whisper')).toBeVisible()
    await page.getByText('Canary').click()

    // Close menu by clicking outside
    await page.locator('.v-overlay__scrim').click({ force: true })

    // Reopen and verify Canary is still selected
    await page.locator('button:has(.mdi-subtitles-outline)').first().click()
    const canaryRadio = page.locator('.v-radio').filter({ hasText: 'Canary' })
    await expect(canaryRadio.locator('.mdi-radiobox-marked')).toBeVisible()
  })

  test('enable transcription changes icon color and shows panel button', async ({ page, context }) => {
    await context.grantPermissions(['camera', 'microphone'])
    await page.goto(`/tenant/${tenantId}/room/${roomId}/call`)

    await page.getByRole('button', { name: /join/i }).click()
    await expect(page.getByText('Chat')).toBeVisible({ timeout: 15000 })

    // Open menu and toggle transcription on
    await page.locator('button:has(.mdi-subtitles-outline)').first().click()
    await expect(page.getByText('Transcription disabled')).toBeVisible()

    // Enable transcription via the switch
    await page.locator('.v-switch').click()

    // Verify the subtitle icon becomes primary colored (filled icon)
    await expect(page.locator('button:has(.mdi-subtitles)')).toBeVisible()

    // Verify transcript panel toggle button appears
    await expect(page.locator('button:has(.mdi-text-box-outline), button:has(.mdi-text-box)')).toBeVisible()
  })

  test('transcript panel shows empty state', async ({ page, context }) => {
    await context.grantPermissions(['camera', 'microphone'])
    await page.goto(`/tenant/${tenantId}/room/${roomId}/call`)

    await page.getByRole('button', { name: /join/i }).click()
    await expect(page.getByText('Chat')).toBeVisible({ timeout: 15000 })

    // Enable transcription
    await page.locator('button:has(.mdi-subtitles-outline)').first().click()
    await page.locator('.v-switch').click()

    // Close menu
    await page.keyboard.press('Escape')

    // Transcript panel should auto-open with empty state
    await expect(page.getByText('No transcript yet')).toBeVisible({ timeout: 5000 })
  })

  test('disable transcription hides panel and overlay', async ({ page, context }) => {
    await context.grantPermissions(['camera', 'microphone'])
    await page.goto(`/tenant/${tenantId}/room/${roomId}/call`)

    await page.getByRole('button', { name: /join/i }).click()
    await expect(page.getByText('Chat')).toBeVisible({ timeout: 15000 })

    // Enable transcription
    await page.locator('button:has(.mdi-subtitles-outline)').first().click()
    await page.locator('.v-switch').click()
    await page.keyboard.press('Escape')

    // Verify transcript panel is visible
    await expect(page.getByText('No transcript yet')).toBeVisible({ timeout: 5000 })

    // Disable transcription
    await page.locator('button:has(.mdi-subtitles)').first().click()
    await page.locator('.v-switch').click()
    await page.keyboard.press('Escape')

    // Transcript panel should be hidden
    await expect(page.getByText('No transcript yet')).not.toBeVisible()
  })

  test('display name shows actual name with (You) suffix', async ({ page, context }) => {
    await context.grantPermissions(['camera', 'microphone'])
    await page.goto(`/tenant/${tenantId}/room/${roomId}/call`)

    await page.getByRole('button', { name: /join/i }).click()
    await expect(page.getByText('Chat')).toBeVisible({ timeout: 15000 })

    // Should show "E2E User N (You)" -- not "You (You)"
    await expect(page.getByText(`${user.displayName} (You)`)).toBeVisible({ timeout: 5000 })
    await expect(page.getByText('You (You)')).not.toBeVisible()
  })
})
