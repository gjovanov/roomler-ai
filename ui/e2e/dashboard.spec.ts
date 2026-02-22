import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Dashboard', () => {
  test('dashboard shows create workspace form when no tenants', async ({ page }) => {
    const user = uniqueUser()
    await registerUserViaApi(user)
    await loginViaUi(page, user.username, user.password)
    await expect(page.getByText(/create your first workspace/i)).toBeVisible({ timeout: 10000 })
  })

  test('dashboard shows tenant cards when tenants exist', async ({ page }) => {
    const user = uniqueUser()
    const result = await registerUserViaApi(user)
    await createTenantViaApi(result.access_token, 'My Org', `org-${Date.now()}`)
    await loginViaUi(page, user.username, user.password)
    await expect(page.getByText('My Org').first()).toBeVisible({ timeout: 10000 })
  })

  test('tenant dashboard shows stats and quick actions', async ({ page }) => {
    const user = uniqueUser()
    const result = await registerUserViaApi(user)
    const tenant = await createTenantViaApi(
      result.access_token,
      'Stats Org',
      `stats-${Date.now()}`,
    )
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenant.id}`)
    await expect(page.getByText(/rooms/i).first()).toBeVisible({ timeout: 10000 })
    await expect(page.getByText(/new room/i)).toBeVisible()
    await expect(page.getByText(/start meeting/i)).toBeVisible()
  })
})
