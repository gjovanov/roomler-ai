import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('404 Not Found', () => {
  test('non-existent route shows 404 page', async ({ page }) => {
    await page.goto('/nonexistent-page-12345')

    await expect(page.getByText('404')).toBeVisible({ timeout: 10000 })
    await expect(page.getByText(/page not found/i)).toBeVisible()
  })

  test('404 page shows descriptive message', async ({ page }) => {
    await page.goto('/this-does-not-exist')

    await expect(page.getByText('404')).toBeVisible({ timeout: 10000 })
    await expect(page.getByText(/does not exist or has been moved/i)).toBeVisible()
  })

  test('404 page has "Go to Dashboard" button', async ({ page }) => {
    await page.goto('/some-random-route')

    await expect(page.getByText('404')).toBeVisible({ timeout: 10000 })
    await expect(page.getByRole('link', { name: /go to dashboard/i })).toBeVisible()
  })

  test('clicking "Go to Dashboard" navigates to root', async ({ page }) => {
    await page.goto('/unknown-path')

    await expect(page.getByText('404')).toBeVisible({ timeout: 10000 })

    await page.getByRole('link', { name: /go to dashboard/i }).click()

    // Should navigate to / (which may redirect to /login or /landing if unauthenticated)
    await expect(page).toHaveURL(/^\/(login|landing)?$/, { timeout: 5000 })
  })

  test('authenticated user clicking "Go to Dashboard" goes to dashboard', async ({ page }) => {
    const user = uniqueUser()
    await registerUserViaApi(user)
    await loginViaUi(page, user.username, user.password)

    // Navigate to a non-existent route
    await page.goto('/does-not-exist-xyz')

    await expect(page.getByText('404')).toBeVisible({ timeout: 10000 })

    await page.getByRole('link', { name: /go to dashboard/i }).click()

    // Authenticated user should land on dashboard
    await expect(page).toHaveURL('/', { timeout: 5000 })
  })

  test('deep non-existent route shows 404', async ({ page }) => {
    await page.goto('/tenant/fake-id/room/fake-room/nonexistent')

    await expect(page.getByText('404')).toBeVisible({ timeout: 10000 })
  })
})
