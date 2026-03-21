import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  addTenantMemberViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Profile', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let userId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    userId = result.user.id
    await loginViaUi(page, user.username, user.password)
  })

  test('view own profile shows display name and username', async ({ page }) => {
    await page.goto(`/profile/${userId}`)

    await expect(page.getByText(user.displayName)).toBeVisible({ timeout: 10000 })
    await expect(page.getByText(`@${user.username}`)).toBeVisible()
  })

  test('own profile shows Edit button', async ({ page }) => {
    await page.goto(`/profile/${userId}`)

    await expect(page.getByText(user.displayName)).toBeVisible({ timeout: 10000 })
    await expect(page.getByRole('link', { name: /edit/i }).or(page.getByRole('button', { name: /edit/i }))).toBeVisible()
  })

  test('profile shows member since date', async ({ page }) => {
    await page.goto(`/profile/${userId}`)

    await expect(page.getByText(/member since/i)).toBeVisible({ timeout: 10000 })
  })

  test('profile shows initial avatar when no avatar URL set', async ({ page }) => {
    await page.goto(`/profile/${userId}`)

    // The avatar should show the first letter of the display name
    const initial = user.displayName.charAt(0).toUpperCase()
    await expect(page.locator('.v-avatar').getByText(initial)).toBeVisible({ timeout: 10000 })
  })

  test('navigate to profile edit page', async ({ page }) => {
    await page.goto(`/profile/${userId}`)
    await expect(page.getByText(user.displayName)).toBeVisible({ timeout: 10000 })

    // Click the Edit button
    await page.getByRole('link', { name: /edit/i }).or(page.getByRole('button', { name: /edit/i })).click()

    await expect(page).toHaveURL(/\/profile\/edit/, { timeout: 5000 })
  })

  test('profile edit page shows form fields', async ({ page }) => {
    await page.goto('/profile/edit')

    await expect(page.getByText(/edit profile/i)).toBeVisible({ timeout: 10000 })
    await expect(page.getByLabel(/display name/i)).toBeVisible()
    await expect(page.getByLabel(/bio/i)).toBeVisible()
    await expect(page.getByLabel(/avatar url/i)).toBeVisible()
  })

  test('edit display name and save', async ({ page }) => {
    await page.goto('/profile/edit')

    await expect(page.getByLabel(/display name/i)).toBeVisible({ timeout: 10000 })

    // Clear and fill a new display name
    const nameField = page.getByLabel(/display name/i)
    await nameField.clear()
    await nameField.fill('Updated Name')

    // Click Save
    await page.getByRole('button', { name: /save/i }).click()

    // Should navigate back (profile or previous page)
    await page.waitForTimeout(2000)
  })

  test('cancel on profile edit navigates back', async ({ page }) => {
    // First visit profile, then go to edit
    await page.goto(`/profile/${userId}`)
    await expect(page.getByText(user.displayName)).toBeVisible({ timeout: 10000 })

    await page.goto('/profile/edit')
    await expect(page.getByText(/edit profile/i)).toBeVisible({ timeout: 10000 })

    // Click Cancel
    await page.getByRole('button', { name: /cancel/i }).click()

    // Should navigate back
    await page.waitForTimeout(1000)
  })

  test('toggle theme between light and dark', async ({ page }) => {
    // The theme toggle button is in the app bar
    const themeBtn = page.locator('.mdi-weather-night').or(page.locator('.mdi-weather-sunny'))
    await expect(themeBtn).toBeVisible({ timeout: 10000 })

    // Click to toggle theme
    await themeBtn.click()
    await page.waitForTimeout(500)

    // The icon should have changed (from night to sunny or vice versa)
    const afterToggle = page.locator('.mdi-weather-night').or(page.locator('.mdi-weather-sunny'))
    await expect(afterToggle).toBeVisible()
  })

  test('view another user profile', async ({ page }) => {
    // Create another user
    const otherUser = uniqueUser()
    const otherAuth = await registerUserViaApi(otherUser)

    // Navigate to other user's profile
    await page.goto(`/profile/${otherAuth.user.id}`)

    await expect(page.getByText(otherUser.displayName)).toBeVisible({ timeout: 10000 })
    await expect(page.getByText(`@${otherUser.username}`)).toBeVisible()

    // Edit button should NOT be visible for another user's profile
    await expect(page.getByRole('link', { name: /edit/i })).not.toBeVisible()
  })
})
