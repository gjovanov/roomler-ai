import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createConferenceViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Conference List', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string

  test.beforeEach(async ({ page }) => {
    user = uniqueUser()
    const result = await registerUserViaApi(user)
    token = result.access_token
    const tenant = await createTenantViaApi(token, 'Conf List Org', `conflist-${Date.now()}`)
    tenantId = tenant.id

    await loginViaUi(page, user.username, user.password)
  })

  test('conferences page shows created conferences', async ({ page }) => {
    // Create 2 conferences via API
    const conf1 = await createConferenceViaApi(token, tenantId, 'Weekly Standup')
    const conf2 = await createConferenceViaApi(token, tenantId, 'Design Review')

    await page.goto(`/tenant/${tenantId}/conferences`)

    // Both conferences should appear
    await expect(page.getByText('Weekly Standup')).toBeVisible({ timeout: 10000 })
    await expect(page.getByText('Design Review')).toBeVisible()
  })

  test('clicking a conference navigates to conference view', async ({ page }) => {
    const conf = await createConferenceViaApi(token, tenantId, 'Navigate Test')

    await page.goto(`/tenant/${tenantId}/conferences`)
    await expect(page.getByText('Navigate Test')).toBeVisible({ timeout: 10000 })

    // Click the conference item
    await page.getByText('Navigate Test').click()

    // Should navigate to conference view
    await expect(page).toHaveURL(new RegExp(`/tenant/${tenantId}/conference/${conf.id}`), {
      timeout: 10000,
    })
  })

  test('empty state is shown when no conferences exist', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/conferences`)

    await expect(page.getByText(/no conferences/i)).toBeVisible({ timeout: 10000 })
  })

  test('create conference button opens dialog', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/conferences`)

    // Click create button
    await page.getByRole('button', { name: /create conference/i }).click()

    // Dialog should appear with subject field
    await expect(page.getByLabel('Subject')).toBeVisible({ timeout: 5000 })
  })

  test('sidebar has Conferences link', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}/conferences`)

    // The sidebar should have a Conferences nav item
    await expect(page.getByRole('link', { name: /conferences/i })).toBeVisible({ timeout: 10000 })
  })

  test('dashboard conference card links to conferences page', async ({ page }) => {
    await page.goto(`/tenant/${tenantId}`)

    // The conference card should be clickable
    const confCard = page.getByText('Conferences').first()
    await expect(confCard).toBeVisible({ timeout: 10000 })

    // Click the card area
    await confCard.click()

    // Should navigate to conferences list
    await expect(page).toHaveURL(new RegExp(`/tenant/${tenantId}/conferences`), {
      timeout: 10000,
    })
  })
})
