import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

const API_URL = process.env.E2E_API_URL || 'http://localhost:5001'

test.describe('Billing & Subscription', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let tenantId: string

  test.beforeEach(async () => {
    user = uniqueUser()
    const auth = await registerUserViaApi(user)
    token = auth.access_token
    const tenant = await createTenantViaApi(token, 'Billing Org', `billing-${Date.now()}`)
    tenantId = tenant.id
  })

  test('billing page displays current plan as Free by default', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing`)

    // Should show "Current Plan" label and "free" plan
    await expect(page.getByText('Current Plan')).toBeVisible({ timeout: 10000 })
    await expect(page.getByText('free').first()).toBeVisible({ timeout: 5000 })
  })

  test('billing page displays all three plan cards', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing`)

    // Wait for plans to load from /api/stripe/plans
    await expect(page.getByText('Available Plans')).toBeVisible({ timeout: 10000 })

    // Verify all three plan cards are visible
    await expect(page.getByText('Free').first()).toBeVisible()
    await expect(page.getByText('Pro').first()).toBeVisible()
    await expect(page.getByText('Business').first()).toBeVisible()
  })

  test('plan cards show correct prices', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing`)

    await expect(page.getByText('Available Plans')).toBeVisible({ timeout: 10000 })

    // FR-115 — per user, in the currency the toggle picks. EUR is on sale
    // only where the server's `stripe.currencies` names it, and the toggle
    // shows only when there is a choice. USD is pinned when it does, so the
    // amounts do not depend on the runner's time zone.
    const plans = (await (await fetch(`${API_URL}/api/stripe/plans`)).json()) as Array<{
      prices: Array<{ currency: string }>
    }>
    const eurOnSale = plans.some((p) => p.prices.some((x) => x.currency === 'eur'))
    const toggle = page.getByTestId('currency-toggle')
    if (eurOnSale) await toggle.getByRole('button', { name: 'USD' }).click()
    else await expect(toggle).toHaveCount(0)
    await expect(page.getByTestId('price-free')).toContainText('$0')
    await expect(page.getByTestId('price-pro')).toContainText('$8')
    await expect(page.getByTestId('price-pro')).toContainText('per user / month')
    await expect(page.getByTestId('price-business')).toContainText('$16')
    if (!eurOnSale) return

    // The same plans in EUR: the same number.
    await toggle.getByRole('button', { name: 'EUR' }).click()
    await expect(page.getByTestId('price-pro')).toContainText('€8')
    await expect(page.getByTestId('price-business')).toContainText('€16')
  })

  test('the Free plan says commercial use is allowed (FR-115)', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing`)

    await expect(page.getByText('Available Plans')).toBeVisible({ timeout: 10000 })
    await expect(page.getByTestId('commercial-use')).toHaveText('Free for commercial use')
    await expect(page.getByTestId('plan-card-free')).toContainText('Up to 10')
    await expect(page.getByTestId('plan-card-free')).toContainText('100')
  })

  test('plan cards display limits (users, devices, sessions, relay, meetings)', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing`)

    await expect(page.getByText('Available Plans')).toBeVisible({ timeout: 10000 })

    // FR-115 — the labels lead with what costs money or grows with the
    // org. 'AI Recognition' was removed with the feature (FR-32 #898), and
    // 'Members'/'Channels' became 'Users' and unlimited chat in FR-115: a
    // spec cannot tell a removal from a breakage, so it is updated, not
    // relaxed.
    await expect(page.getByText('Users').first()).toBeVisible()
    await expect(page.getByText('Devices').first()).toBeVisible()
    await expect(page.getByText('Remote sessions at a time').first()).toBeVisible()
    await expect(page.getByText('Relay at full speed').first()).toBeVisible()
    await expect(page.getByText('Meetings').first()).toBeVisible()
    await expect(page.getByText('Storage').first()).toBeVisible()
    await expect(page.getByText('Recordings').first()).toBeVisible()
    await expect(page.getByTestId('plan-card-pro')).toContainText('1 per user')
    await expect(page.getByTestId('plan-card-free')).toContainText('60 min')
  })

  test('free plan card shows "Current Plan" button disabled', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing`)

    await expect(page.getByText('Available Plans')).toBeVisible({ timeout: 10000 })

    // The free plan card should have a disabled "Current Plan" button
    const currentPlanBtn = page.getByRole('button', { name: 'Current Plan' })
    await expect(currentPlanBtn).toBeVisible()
    await expect(currentPlanBtn).toBeDisabled()
  })

  test('upgrade buttons are visible for Pro and Business plans', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing`)

    await expect(page.getByText('Available Plans')).toBeVisible({ timeout: 10000 })

    // Should have "Upgrade to Pro" and "Upgrade to Business" buttons
    await expect(page.getByRole('button', { name: /upgrade to pro/i })).toBeVisible()
    await expect(page.getByRole('button', { name: /upgrade to business/i })).toBeVisible()
  })

  test('manage subscription button is hidden when no billing account', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing`)

    await expect(page.getByText('Current Plan')).toBeVisible({ timeout: 10000 })

    // "Manage Subscription" should NOT be visible for a free plan with no billing
    await expect(page.getByRole('button', { name: /manage subscription/i })).not.toBeVisible()
  })

  test('billing page handles success query param', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing?success=true`)

    // Should show success alert
    await expect(
      page.getByText(/subscription updated successfully/i),
    ).toBeVisible({ timeout: 10000 })
  })

  test('billing page handles canceled query param', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing?canceled=true`)

    // Should show canceled info alert
    await expect(
      page.getByText(/checkout was canceled/i),
    ).toBeVisible({ timeout: 10000 })
  })

  test('GET /api/stripe/plans returns correct plan data', async () => {
    // This is a public endpoint, no auth required
    const resp = await fetch(`${API_URL}/api/stripe/plans`)
    expect(resp.ok).toBeTruthy()

    const plans = (await resp.json()) as Array<{
      id: string
      name: string
      price_cents: number
      // FR-115 — per seat, monthly/annual, USD/EUR; Free allows commercial use.
      prices: Array<{ interval: string; currency: string; unit_amount: number }>
      per_seat: boolean
      commercial_use: boolean
      features: string[]
      limits: {
        max_members: number
        max_channels: number
        video_max_participants: number
        meeting_max_minutes: number
        cloud_integrations: boolean
        recordings: boolean
        // Kept in lockstep with crates/db tenant.rs::Plan::limits and
        // crates/tests/src/billing_tests.rs.
        max_devices: number
        max_tunnel_clients: number
        overlay_mesh: boolean
        exit_nodes: boolean
        magic_dns: boolean
        max_concurrent_sessions: number
        relay_bytes_per_month: number
        storage_bytes: number
        per_seat: boolean
      }
    }>

    expect(plans).toHaveLength(3)
    const UNLIMITED = 4294967295
    const GB = 1_000_000_000
    const GIB = 1024 ** 3

    // Free plan
    const free = plans.find((p) => p.id === 'free')!
    expect(free).toBeDefined()
    expect(free.name).toBe('Free')
    expect(free.price_cents).toBe(0)
    expect(free.commercial_use).toBe(true)
    expect(free.per_seat).toBe(false)
    expect(free.features[0]).toBe('Free for commercial use')
    expect(free.limits.max_members).toBe(10)
    expect(free.limits.max_channels).toBe(UNLIMITED)
    expect(free.limits.video_max_participants).toBe(10)
    expect(free.limits.meeting_max_minutes).toBe(60)
    expect(free.limits.cloud_integrations).toBe(false)
    expect(free.limits.recordings).toBe(false)
    expect(free.limits.max_devices).toBe(100)
    expect(free.limits.max_tunnel_clients).toBe(100)
    expect(free.limits.overlay_mesh).toBe(true)
    expect(free.limits.exit_nodes).toBe(false)
    expect(free.limits.magic_dns).toBe(true)
    expect(free.limits.max_concurrent_sessions).toBe(1)
    expect(free.limits.relay_bytes_per_month).toBe(10 * GB)
    expect(free.limits.storage_bytes).toBe(GIB)

    // Pro plan — one seat's numbers
    const pro = plans.find((p) => p.id === 'pro')!
    expect(pro).toBeDefined()
    expect(pro.name).toBe('Pro')
    expect(pro.price_cents).toBe(800)
    expect(pro.per_seat).toBe(true)
    expect(pro.prices).toContainEqual({ interval: 'month', currency: 'usd', unit_amount: 800 })
    // EUR only where the server puts it on sale (`stripe.currencies`), and
    // then at the same number.
    for (const p of pro.prices.filter((x) => x.interval === 'month' && x.currency === 'eur')) {
      expect(p.unit_amount).toBe(800)
    }
    for (const p of pro.prices.filter((x) => x.interval === 'year')) {
      expect(p.unit_amount).toBe(8000) // two months free
    }
    expect(pro.limits.video_max_participants).toBe(50)
    expect(pro.limits.cloud_integrations).toBe(true)
    expect(pro.limits.max_devices).toBe(1000)
    expect(pro.limits.max_tunnel_clients).toBe(1000)
    expect(pro.limits.exit_nodes).toBe(true)
    expect(pro.limits.magic_dns).toBe(true)
    expect(pro.limits.max_concurrent_sessions).toBe(1)
    expect(pro.limits.relay_bytes_per_month).toBe(25 * GB)
    expect(pro.limits.storage_bytes).toBe(10 * GIB)

    // Business plan
    const biz = plans.find((p) => p.id === 'business')!
    expect(biz).toBeDefined()
    expect(biz.name).toBe('Business')
    expect(biz.price_cents).toBe(1600)
    expect(biz.limits.video_max_participants).toBe(100)
    expect(biz.limits.recordings).toBe(true)
    expect(biz.limits.max_devices).toBe(5000)
    expect(biz.limits.max_tunnel_clients).toBe(5000)
    expect(biz.limits.exit_nodes).toBe(true)
    expect(biz.limits.magic_dns).toBe(true)
    expect(biz.limits.max_concurrent_sessions).toBe(UNLIMITED)
    expect(biz.limits.relay_bytes_per_month).toBe(100 * GB)
    for (const p of plans) {
      expect(p.features.join(' ')).not.toMatch(/\bAI\b/) // deleted in FR-32
    }
  })

  test('change-plan refuses an org with no subscription (FR-115)', async () => {
    const resp = await fetch(`${API_URL}/api/stripe/change-plan`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({ tenant_id: tenantId, plan: 'business', interval: 'month' }),
    })
    expect(resp.status).toBe(400)
  })

  test('the billing summary counts the org’s seats (FR-115)', async () => {
    const resp = await fetch(`${API_URL}/api/stripe/summary?tenant_id=${tenantId}`, {
      headers: { Authorization: `Bearer ${token}` },
    })
    expect(resp.ok).toBeTruthy()
    const s = (await resp.json()) as { plan: string; seats_now: number; subscribed: boolean }
    expect(s.plan).toBe('free')
    expect(s.seats_now).toBe(1)
    expect(s.subscribed).toBe(false)
  })

  test('checkout rejects an unknown interval and currency (FR-115)', async () => {
    for (const extra of [{ interval: 'fortnight' }, { currency: 'jpy' }]) {
      const resp = await fetch(`${API_URL}/api/stripe/checkout`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${token}`,
        },
        body: JSON.stringify({
          tenant_id: tenantId,
          plan: 'pro',
          success_url: 'http://localhost:5000/billing?success=true',
          cancel_url: 'http://localhost:5000/billing?canceled=true',
          ...extra,
        }),
      })
      expect(resp.status).toBe(400)
    }
  })

  test('checkout endpoint requires authentication', async () => {
    const resp = await fetch(`${API_URL}/api/stripe/checkout`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        tenant_id: tenantId,
        plan: 'pro',
        success_url: 'http://localhost:5000/billing?success=true',
        cancel_url: 'http://localhost:5000/billing?canceled=true',
      }),
    })
    expect(resp.status).toBe(401)
  })

  test('checkout endpoint rejects invalid plan', async () => {
    const resp = await fetch(`${API_URL}/api/stripe/checkout`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({
        tenant_id: tenantId,
        plan: 'nonexistent',
        success_url: 'http://localhost:5000/billing?success=true',
        cancel_url: 'http://localhost:5000/billing?canceled=true',
      }),
    })
    // Should be 400 (bad request) for invalid plan
    expect(resp.status).toBe(400)
  })

  test('portal endpoint requires authentication', async () => {
    const resp = await fetch(`${API_URL}/api/stripe/portal`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        tenant_id: tenantId,
        return_url: 'http://localhost:5000/billing',
      }),
    })
    expect(resp.status).toBe(401)
  })

  test('portal endpoint returns error when no billing account', async () => {
    const resp = await fetch(`${API_URL}/api/stripe/portal`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({
        tenant_id: tenantId,
        return_url: 'http://localhost:5000/billing',
      }),
    })
    // Should fail because free tenant has no billing account / customer_id
    expect(resp.status).toBe(400)
  })

  test('webhook endpoint rejects missing signature', async () => {
    const resp = await fetch(`${API_URL}/api/stripe/webhook`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        type: 'checkout.session.completed',
        data: { object: {} },
      }),
    })
    expect(resp.status).toBe(400)
  })

  test('webhook endpoint rejects invalid signature', async () => {
    const resp = await fetch(`${API_URL}/api/stripe/webhook`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'stripe-signature': 't=1234567890,v1=invalidsignature',
      },
      body: JSON.stringify({
        type: 'checkout.session.completed',
        data: { object: {} },
      }),
    })
    expect(resp.status).toBe(401)
  })

  test('non-owner cannot access checkout endpoint for tenant', async () => {
    // Register a second user who is NOT the tenant owner
    const otherUser = uniqueUser()
    const otherAuth = await registerUserViaApi(otherUser)

    const resp = await fetch(`${API_URL}/api/stripe/checkout`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${otherAuth.access_token}`,
      },
      body: JSON.stringify({
        tenant_id: tenantId,
        plan: 'pro',
        success_url: 'http://localhost:5000/billing?success=true',
        cancel_url: 'http://localhost:5000/billing?canceled=true',
      }),
    })
    // Should be 403 (forbidden) — not the tenant owner and no MANAGE_TENANT permission
    expect(resp.status).toBe(403)
  })
})
