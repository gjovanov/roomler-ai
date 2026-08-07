import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

/**
 * Observability wave 1+2 surfaces.
 *
 * These lock the GATING and the shape of each view, not the numbers: a
 * fresh e2e tenant has no telemetry history, so every chart legitimately
 * renders its empty state. What must hold regardless of data is that a
 * plain member sees the member-safe panels, an org owner reaches
 * Analytics, and nobody who isn't on the platform allowlist can see the
 * platform view — the fail-closed behaviour the API's 404s depend on.
 */

async function newOrgOwner(page: import('@playwright/test').Page) {
  const user = uniqueUser()
  const result = await registerUserViaApi(user)
  const tenant = await createTenantViaApi(
    result.access_token,
    'Obs Org',
    `obs-${Date.now()}-${Math.floor(Math.random() * 1e4)}`,
  )
  await loginViaUi(page, user.username, user.password)
  return { user, tenant, token: result.access_token }
}

test.describe('Org dashboard — Insights + Network', () => {
  test('insights panel renders for the org, with empty states not errors', async ({ page }) => {
    const { tenant } = await newOrgOwner(page)
    await page.goto(`/tenant/${tenant.id}`)

    await expect(page.getByText('Insights')).toBeVisible({ timeout: 15000 })
    await expect(page.getByText(/machines online/i)).toBeVisible()
    await expect(page.getByText(/call minutes/i)).toBeVisible()
    // A brand-new org has no samples yet — the panel must SAY so rather
    // than render a misleading flat line at zero.
    await expect(page.getByText(/no samples yet|no calls in the last/i).first()).toBeVisible()
  })

  test('mesh graph is absent for an org with no devices', async ({ page }) => {
    const { tenant } = await newOrgOwner(page)
    await page.goto(`/tenant/${tenant.id}`)
    await expect(page.getByText('Insights')).toBeVisible({ timeout: 15000 })
    // The Network card only renders once the org HAS overlay nodes —
    // an empty ring would be noise on a fresh workspace.
    await expect(page.getByText('Network', { exact: true })).toHaveCount(0)
  })
})

test.describe('Org analytics', () => {
  test('owner reaches Analytics and can switch ranges', async ({ page }) => {
    const { tenant } = await newOrgOwner(page)
    await page.goto(`/tenant/${tenant.id}/analytics`)

    await expect(page.getByRole('heading', { name: 'Analytics' })).toBeVisible({ timeout: 15000 })
    // Owner is never fail-closed out, even before the permission mask
    // has loaded.
    await expect(page.getByText(/requires an org admin/i)).toHaveCount(0)

    await expect(page.getByRole('tab', { name: /machines/i })).toBeVisible()
    await expect(page.getByRole('tab', { name: /calls/i })).toBeVisible()
    await expect(page.getByRole('tab', { name: /tunnels/i })).toBeVisible()

    await page.getByRole('button', { name: '30D', exact: true }).click()
    await expect(page.getByText(/machines online/i).first()).toBeVisible()

    await page.getByRole('tab', { name: /calls/i }).click()
    await expect(page.getByText(/participant-minutes/i).first()).toBeVisible({ timeout: 10000 })
  })
})

test.describe('Platform observability', () => {
  test('a non-platform-admin gets the limited notice, never a logout', async ({ page }) => {
    const { user } = await newOrgOwner(page)
    await page.goto('/observability')

    // Fail-closed: the page renders its notice and — critically — does
    // NOT bounce to /login. A 403 from the API would wipe the session,
    // which is exactly why these endpoints answer 404 and the view
    // gates client-side before fetching.
    await expect(page.getByText(/limited to platform operators/i)).toBeVisible({ timeout: 15000 })
    await expect(page).toHaveURL(/\/observability/)

    // Still authenticated afterwards: navigating back into the app works.
    await page.goto('/')
    await expect(page).not.toHaveURL(/\/login/)
    expect(user.username).toBeTruthy()
  })

  test('the platform nav item is hidden for ordinary users', async ({ page }) => {
    const { tenant } = await newOrgOwner(page)
    await page.goto(`/tenant/${tenant.id}`)
    await expect(page.getByText('Insights')).toBeVisible({ timeout: 15000 })
    await expect(page.getByRole('link', { name: /observability/i })).toHaveCount(0)
  })
})
