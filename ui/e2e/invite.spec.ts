import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createInviteViaApi,
  revokeInviteViaApi,
  loginViaUi,
  registerViaUi,
} from './fixtures/test-helpers'

test.describe('Invite functionality', () => {
  test('invite landing page shows tenant info for unauthenticated user', async ({ page }) => {
    // Setup: create tenant + invite via API
    const owner = uniqueUser()
    const ownerAuth = await registerUserViaApi(owner)
    const tenant = await createTenantViaApi(ownerAuth.access_token, 'Invite Test Org', 'invite-test-org')
    const invite = await createInviteViaApi(ownerAuth.access_token, tenant.id)

    // Navigate to invite page (unauthenticated)
    await page.goto(`/invite/${invite.code}`)

    // Should see tenant name and inviter
    await expect(page.getByText("You're invited!")).toBeVisible({ timeout: 5000 })
    await expect(page.getByText('Invite Test Org')).toBeVisible()

    // Should see Register and Login buttons (not authenticated)
    await expect(page.getByRole('link', { name: /register/i })).toBeVisible()
    await expect(page.getByRole('link', { name: /login/i })).toBeVisible()
  })

  test('register via invite link auto-joins tenant', async ({ page }) => {
    // Setup
    const owner = uniqueUser()
    const ownerAuth = await registerUserViaApi(owner)
    const tenant = await createTenantViaApi(ownerAuth.access_token, 'Register Join Org', 'reg-join-org')
    const invite = await createInviteViaApi(ownerAuth.access_token, tenant.id)

    // Visit invite page
    await page.goto(`/invite/${invite.code}`)
    await expect(page.getByText("You're invited!")).toBeVisible({ timeout: 5000 })

    // Click Register
    await page.getByRole('link', { name: /register/i }).click()
    await expect(page).toHaveURL(/\/register\?invite=/, { timeout: 5000 })

    // Fill registration form
    const newUser = uniqueUser()
    const inputs = page.locator('input')
    await inputs.nth(0).fill(newUser.email)
    await inputs.nth(1).fill(newUser.username)
    await inputs.nth(2).fill(newUser.displayName)
    await page.locator('input[type="password"]').fill(newUser.password)
    await page.getByRole('button', { name: /register/i }).click()

    // Should redirect to tenant dashboard (auto-joined via invite_code)
    await expect(page).toHaveURL(new RegExp(`/tenant/${tenant.id}`), { timeout: 10000 })
  })

  test('logged-in user accepts invite and joins tenant', async ({ page }) => {
    // Setup: create tenant + invite
    const owner = uniqueUser()
    const ownerAuth = await registerUserViaApi(owner)
    const tenant = await createTenantViaApi(ownerAuth.access_token, 'Accept Org', 'accept-org')
    const invite = await createInviteViaApi(ownerAuth.access_token, tenant.id)

    // Register another user and log them in
    const joiner = uniqueUser()
    await registerUserViaApi(joiner)
    await loginViaUi(page, joiner.username, joiner.password)

    // Navigate to invite link
    await page.goto(`/invite/${invite.code}`)

    // Should see "Accept & Join" button (authenticated + valid)
    await expect(page.getByRole('button', { name: /accept/i })).toBeVisible({ timeout: 5000 })

    // Click accept
    await page.getByRole('button', { name: /accept/i }).click()

    // Should redirect to tenant
    await expect(page).toHaveURL(new RegExp(`/tenant/${tenant.id}`), { timeout: 10000 })
  })

  test('login via invite link redirects to invite page', async ({ page }) => {
    // Setup
    const owner = uniqueUser()
    const ownerAuth = await registerUserViaApi(owner)
    const tenant = await createTenantViaApi(ownerAuth.access_token, 'Login Join Org', 'login-join-org')
    const invite = await createInviteViaApi(ownerAuth.access_token, tenant.id)

    // Register a user (via API, not logged in on browser)
    const existing = uniqueUser()
    await registerUserViaApi(existing)

    // Visit invite page (unauthenticated)
    await page.goto(`/invite/${invite.code}`)
    await expect(page.getByText("You're invited!")).toBeVisible({ timeout: 5000 })

    // Click Login
    await page.getByRole('link', { name: /login/i }).click()
    await expect(page).toHaveURL(/\/login\?invite=/, { timeout: 5000 })

    // Login
    await page.locator('input').first().fill(existing.username)
    await page.locator('input[type="password"]').fill(existing.password)
    await page.getByRole('button', { name: /login/i }).click()

    // Should redirect back to invite page (sessionStorage has the code)
    await expect(page).toHaveURL(new RegExp(`/invite/${invite.code}`), { timeout: 10000 })

    // Now they should see "Accept & Join"
    await expect(page.getByRole('button', { name: /accept/i })).toBeVisible({ timeout: 5000 })
  })

  test('revoked invite shows error state', async ({ page }) => {
    // Setup
    const owner = uniqueUser()
    const ownerAuth = await registerUserViaApi(owner)
    const tenant = await createTenantViaApi(ownerAuth.access_token, 'Revoked Org', 'revoked-org')
    const invite = await createInviteViaApi(ownerAuth.access_token, tenant.id)

    // Revoke the invite
    await revokeInviteViaApi(ownerAuth.access_token, tenant.id, invite.id)

    // Visit the revoked invite page
    await page.goto(`/invite/${invite.code}`)

    // Should show the invite but with warning about invalid status
    await expect(page.getByText(/no longer valid/i)).toBeVisible({ timeout: 5000 })
  })
})
