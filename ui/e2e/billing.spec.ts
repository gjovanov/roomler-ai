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

    // Free: $0, Pro: $8, Business: $16
    await expect(page.getByText('$0').first()).toBeVisible()
    await expect(page.getByText('$8').first()).toBeVisible()
    await expect(page.getByText('$16').first()).toBeVisible()
  })

  test('plan cards display limits (members, channels, video participants)', async ({ page }) => {
    await loginViaUi(page, user.username, user.password)
    await page.goto(`/tenant/${tenantId}/billing`)

    await expect(page.getByText('Available Plans')).toBeVisible({ timeout: 10000 })

    // Verify plan limit labels exist
    await expect(page.getByText('Members').first()).toBeVisible()
    await expect(page.getByText('Channels').first()).toBeVisible()
    await expect(page.getByText('Video Participants').first()).toBeVisible()
    await expect(page.getByText('Cloud Integrations').first()).toBeVisible()
    await expect(page.getByText('AI Recognition').first()).toBeVisible()
    await expect(page.getByText('Recordings').first()).toBeVisible()
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
      limits: {
        max_members: number
        max_channels: number
        video_max_participants: number
        cloud_integrations: boolean
        ai_recognition: boolean
        recordings: boolean
        // S5 pivot — per-user pricing with device caps; kept in
        // lockstep with crates/db tenant.rs::Plan::limits and
        // crates/tests/src/billing_tests.rs.
        max_devices: number
        max_tunnel_clients: number
        overlay_mesh: boolean
        exit_nodes: boolean
        magic_dns: boolean
        max_concurrent_sessions: number
      }
    }>

    expect(plans).toHaveLength(3)

    // Free plan
    const free = plans.find((p) => p.id === 'free')!
    expect(free).toBeDefined()
    expect(free.name).toBe('Free')
    expect(free.price_cents).toBe(0)
    expect(free.limits.max_members).toBe(10)
    expect(free.limits.max_channels).toBe(5)
    expect(free.limits.video_max_participants).toBe(0)
    expect(free.limits.cloud_integrations).toBe(false)
    expect(free.limits.ai_recognition).toBe(false)
    expect(free.limits.recordings).toBe(false)
    expect(free.limits.max_devices).toBe(3)
    expect(free.limits.max_tunnel_clients).toBe(3)
    expect(free.limits.overlay_mesh).toBe(true)
    expect(free.limits.exit_nodes).toBe(false)
    expect(free.limits.magic_dns).toBe(false)
    expect(free.limits.max_concurrent_sessions).toBe(1)

    // Pro plan
    const pro = plans.find((p) => p.id === 'pro')!
    expect(pro).toBeDefined()
    expect(pro.name).toBe('Pro')
    expect(pro.price_cents).toBe(800)
    expect(pro.limits.video_max_participants).toBe(10)
    expect(pro.limits.cloud_integrations).toBe(true)
    expect(pro.limits.max_devices).toBe(30)
    expect(pro.limits.max_tunnel_clients).toBe(30)
    expect(pro.limits.exit_nodes).toBe(true)
    expect(pro.limits.magic_dns).toBe(true)
    expect(pro.limits.max_concurrent_sessions).toBe(5)

    // Business plan
    const biz = plans.find((p) => p.id === 'business')!
    expect(biz).toBeDefined()
    expect(biz.name).toBe('Business')
    expect(biz.price_cents).toBe(1600)
    expect(biz.limits.video_max_participants).toBe(100)
    expect(biz.limits.ai_recognition).toBe(true)
    expect(biz.limits.recordings).toBe(true)
    expect(biz.limits.max_devices).toBe(300)
    expect(biz.limits.max_tunnel_clients).toBe(300)
    expect(biz.limits.exit_nodes).toBe(true)
    expect(biz.limits.magic_dns).toBe(true)
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
