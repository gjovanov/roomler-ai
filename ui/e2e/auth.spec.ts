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

  test('protected deep-link redirects to login, not landing (S2)', async ({ page }) => {
    // A real target (e.g. the desktop app's "View screen" remote URL)
    // must land on the sign-in form with the path stashed for
    // redirect-back — the landing page would strand the link.
    await page.goto('/tenant/000000000000000000000000/agent/000000000000000000000000/remote')
    await expect(page).toHaveURL(/\/login/)
    const stashed = await page.evaluate(() => sessionStorage.getItem('pending_redirect'))
    expect(stashed).toContain('/agent/000000000000000000000000/remote')
  })

  test('navigate between login and register', async ({ page }) => {
    await page.goto('/login')
    await page.getByRole('link', { name: /register/i }).click()
    await expect(page).toHaveURL(/\/register/)

    await page.getByRole('link', { name: /login/i }).click()
    await expect(page).toHaveURL(/\/login/)
  })

  test('an invalid session cookie keeps you out of the app', async ({ page, context }) => {
    const user = uniqueUser()
    await registerUserViaApi(user)

    // ⚠️ Rewritten for cookie-only sessions. The original set
    // `localStorage.access_token` and then tampered it — neither step
    // describes anything any more: the session cookie is HttpOnly since
    // #690/#691 precisely so that page script cannot read or forge it, which
    // is the property that made an XSS unable to walk off with 30 days of
    // re-mintable access. Tampering therefore has to happen through the
    // browser CONTEXT, which is also a truer model of a stolen-or-stale
    // cookie than a localStorage write ever was.
    await loginViaUi(page, user.username, user.password)

    const session = (await context.cookies()).find((c) => c.name === 'access_token')
    expect(session, 'login set no access_token cookie').toBeTruthy()

    await context.clearCookies()
    await context.addCookies([{ ...session!, value: 'expired.invalid.token' }])

    // Navigate to an authenticated route. The property under test is that a
    // bad cookie does NOT get you into the app — not which door you are shown.
    //
    // ⚠️ It can be either: the router guard sends an unauthenticated visitor to
    // /landing, while the 401 interceptor sends them to /login, and which wins
    // is a race. Asserting /login alone made this flaky (it failed on /landing
    // in the 2026-08-30 nightly and passed on retry), so assert the invariant.
    await page.goto('/')
    await expect(page).toHaveURL(/\/(login|landing)/, { timeout: 10000 })
    // And nothing authenticated rendered behind it.
    await expect(page.getByRole('link', { name: 'Rooms' })).toHaveCount(0)
  })

  test('nav menu hides profile/logout when unauthenticated', async ({ page }) => {
    await page.goto('/login')
    // On the login page, AppLayout is not rendered (guest route),
    // so avatar and logout should not be present
    await expect(page.getByText('Logout')).not.toBeVisible()
    await expect(page.getByText('Profile')).not.toBeVisible()
  })
})
