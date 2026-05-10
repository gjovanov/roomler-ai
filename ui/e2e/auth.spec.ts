import { test, expect } from '@playwright/test'
import { uniqueUser, registerUserViaApi, registerViaUi, loginViaUi } from './fixtures/test-helpers'

test.describe('Authentication', () => {
  test('register new user and redirect to dashboard', async ({ page }) => {
    const user = uniqueUser()
    await registerViaUi(page, user.email, user.username, user.displayName, user.password)
    await expect(page).toHaveURL('/')
  })

  test('login with valid credentials', async ({ page }) => {
    const user = uniqueUser()
    // Register via UI first
    await registerViaUi(page, user.email, user.username, user.displayName, user.password)
    // Logout by clearing storage
    await page.evaluate(() => localStorage.clear())
    await page.goto('/login')

    // Login
    await loginViaUi(page, user.username, user.password)
    await expect(page).toHaveURL('/')
  })

  test('login with wrong password shows error', async ({ page }) => {
    await page.goto('/login')
    await page.locator('input').first().fill('nonexistent')
    await page.locator('input[type="password"]').fill('wrongpass')
    await page.getByRole('button', { name: /login/i }).click()
    // Should stay on login page with error
    await expect(page).toHaveURL(/\/login/)
  })

  test('unauthenticated user is redirected to login', async ({ page }) => {
    await page.goto('/')
    // Router beforeEach redirects unauthenticated users to /landing
    // (the marketing/login-prompt page), not directly to /login.
    await expect(page).toHaveURL(/\/landing/)
  })

  test('navigate between login and register', async ({ page }) => {
    await page.goto('/login')
    await page.getByRole('link', { name: /register/i }).click()
    await expect(page).toHaveURL(/\/register/)

    await page.getByRole('link', { name: /login/i }).click()
    await expect(page).toHaveURL(/\/login/)
  })

  test('expired token redirects to login page', async ({ page }) => {
    const user = uniqueUser()
    const creds = await registerUserViaApi(user)

    // Set the valid token so the router guard lets us in
    await page.goto('/login')
    await page.evaluate((token) => {
      localStorage.setItem('access_token', token)
    }, creds.access_token)

    // Now tamper the token to make it invalid
    await page.evaluate(() => {
      localStorage.setItem('access_token', 'expired.invalid.token')
    })

    // Navigate to an authenticated route — the API call will 401
    // and the interceptor should redirect to /login
    await page.goto('/')
    await expect(page).toHaveURL(/\/login/, { timeout: 5000 })
  })

  test('nav menu hides profile/logout when unauthenticated', async ({ page }) => {
    await page.goto('/login')
    // On the login page, AppLayout is not rendered (guest route),
    // so avatar and logout should not be present
    await expect(page.getByText('Logout')).not.toBeVisible()
    await expect(page.getByText('Profile')).not.toBeVisible()
  })
})
