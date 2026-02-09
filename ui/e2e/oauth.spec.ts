import { test, expect } from '@playwright/test'

test.describe('OAuth Login', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/login')
    await page.waitForLoadState('networkidle')
  })

  test('shows all OAuth provider buttons on login page', async ({ page }) => {
    await expect(page.getByText('Or login with')).toBeVisible()

    for (const provider of ['Google', 'Facebook', 'GitHub', 'LinkedIn', 'Microsoft']) {
      await expect(page.getByRole('button', { name: provider })).toBeVisible()
    }
  })

  test('all OAuth provider buttons are clickable', async ({ page }) => {
    for (const provider of ['Google', 'Facebook', 'GitHub', 'LinkedIn', 'Microsoft']) {
      await expect(page.getByRole('button', { name: provider })).toBeEnabled()
    }
  })

  test('clicking Google OAuth button redirects to Google', async ({ page }) => {
    // Intercept the navigation to Google to prevent leaving the page
    await page.route('**/accounts.google.com/**', (route) => route.abort())

    const [request] = await Promise.all([
      page.waitForRequest(
        (req) => req.url().includes('accounts.google.com'),
        { timeout: 10000 },
      ),
      page.getByRole('button', { name: 'Google' }).click(),
    ])

    expect(request.url()).toContain('accounts.google.com')
    expect(request.url()).toContain('client_id=')
    expect(request.url()).toContain('redirect_uri=')
  })

  test('clicking GitHub OAuth button redirects to GitHub', async ({ page }) => {
    await page.route('**/github.com/**', (route) => route.abort())

    const [request] = await Promise.all([
      page.waitForRequest(
        (req) => req.url().includes('github.com/login/oauth'),
        { timeout: 10000 },
      ),
      page.getByRole('button', { name: 'GitHub' }).click(),
    ])

    expect(request.url()).toContain('github.com/login/oauth/authorize')
    expect(request.url()).toContain('client_id=')
  })

  test('OAuth callback page without token shows error', async ({ page }) => {
    await page.goto('/oauth/callback')
    await expect(page.getByText('No token received from OAuth provider')).toBeVisible({ timeout: 5000 })
  })

  test('OAuth callback page with invalid token shows error', async ({ page }) => {
    await page.goto('/oauth/callback?token=invalid-mock-token')
    await expect(page.getByText('Failed to complete OAuth login')).toBeVisible({ timeout: 10000 })
  })
})
