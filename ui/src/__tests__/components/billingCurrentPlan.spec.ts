// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4431 — the current-plan card reads the org's billing from
// `GET /api/stripe/summary` alone. It read the status, the period end,
// "Cancels at period end" and the customer behind "Manage Subscription" from
// the tenant store, which `GET /api/tenant` fills and which never carried
// them, so a paying org saw none of it and had no way to cancel, change its
// card or see its invoices.
import { describe, it, expect, vi, beforeAll, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { mount, flushPromises } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

beforeAll(() => {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver
})

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import BillingView from '@/views/billing/BillingView.vue'
import { api } from '@/api/client'
import { useTenantStore } from '@/stores/tenant'

const TENANT = 'tenant-1'
const limits = {
  max_devices: 100,
  max_tunnel_clients: 10,
  overlay_mesh: true,
  exit_nodes: false,
  magic_dns: true,
  max_concurrent_sessions: 1,
  relay_bytes_per_month: 10_000_000_000,
  max_members: 10,
  max_channels: 10,
  max_message_history: 10_000,
  storage_bytes: 1_000_000_000,
  video_max_participants: 10,
  meeting_max_minutes: 60,
  cloud_integrations: false,
  recordings: false,
}
const plans = ['free', 'pro', 'business'].map((id, i) => ({
  id,
  name: id[0].toUpperCase() + id.slice(1),
  price_cents: i * 800,
  prices: i ? [{ interval: 'month', currency: 'eur', unit_amount: i * 800 }] : [],
  per_seat: i > 0,
  limits,
}))

const free = {
  plan: 'free',
  seats_billed: null,
  seats_now: 1,
  interval: null,
  currency: null,
  status: null,
  current_period_end: null,
  cancel_at_period_end: false,
  subscribed: false,
  has_billing_account: false,
}
// What production answered for the org the page failed (#4431), with the
// billing account the summary now carries.
const cancelling = {
  plan: 'pro',
  seats_billed: 1,
  seats_now: 1,
  interval: 'month',
  currency: 'eur',
  status: 'active',
  current_period_end: '2026-12-25T08:52:55Z',
  cancel_at_period_end: true,
  subscribed: true,
  has_billing_account: true,
}

/** Mount the page for an org whose summary is `summary`, or is refused (a
 * member without MANAGE_TENANT) when `summary` is an Error. */
async function mountWith(summary: object | Error) {
  vi.mocked(api.get).mockImplementation(async (url: string) => {
    if (url === '/stripe/plans') return plans as never
    if (url === `/stripe/summary?tenant_id=${TENANT}`) {
      if (summary instanceof Error) throw summary
      return summary as never
    }
    throw new Error(`unexpected ${url}`)
  })
  // The org as `GET /api/tenant` sends it: no billing.
  useTenantStore().current = { id: TENANT, name: 'Org', slug: 'org', plan: 'pro' }
  const w = mount(BillingView, {
    global: { plugins: [createVuetify({ components, directives })], mocks: { $route: { query: {} } } },
  })
  await flushPromises()
  return w
}

const shown = (w: Awaited<ReturnType<typeof mountWith>>) => ({
  status: w.find('[data-testid="billing-status"]').exists(),
  cancels: w.find('[data-testid="billing-cancels"]').exists(),
  periodEnd: w.find('[data-testid="billing-period-end"]').exists(),
  manage: w.find('[data-testid="manage-subscription"]').exists(),
})

beforeEach(() => {
  setActivePinia(createPinia())
  vi.clearAllMocks()
})

describe('the current-plan card (#4431)', () => {
  it('shows a subscribed org its status, its period end, the cancellation and Manage Subscription', async () => {
    const w = await mountWith(cancelling)

    const chip = w.get('[data-testid="billing-status"]')
    expect(chip.text()).toBe('active')
    expect(chip.classes()).toContain('text-success')
    expect(w.get('[data-testid="billing-cancels"]').text()).toBe('Cancels at period end')
    expect(w.get('[data-testid="billing-period-end"]').text()).toBe(
      `Current period ends: ${new Date('2026-12-25T08:52:55Z').toLocaleDateString()}`,
    )
    const manage = w.get('[data-testid="manage-subscription"]')
    expect(manage.text()).toBe('Manage Subscription')

    // It opens the org's billing portal.
    vi.mocked(api.post).mockResolvedValue({ url: '' } as never)
    await manage.trigger('click')
    await flushPromises()
    expect(api.post).toHaveBeenCalledWith(
      '/stripe/portal',
      expect.objectContaining({ tenant_id: TENANT }),
    )
  })

  it('shows a Free org none of them', async () => {
    const w = await mountWith(free)
    expect(shown(w)).toEqual({ status: false, cancels: false, periodEnd: false, manage: false })
  })

  it('shows a member without MANAGE_TENANT none of them: the summary is refused', async () => {
    const w = await mountWith(new Error('Missing MANAGE_TENANT permission'))
    expect(shown(w)).toEqual({ status: false, cancels: false, periodEnd: false, manage: false })
  })

  it('opens the portal for an org with a billing account and no subscription, with no status', async () => {
    // A checkout that started and was abandoned: the org has a Stripe
    // customer, and the `active` the start recorded describes no subscription.
    const w = await mountWith({ ...free, status: 'active', has_billing_account: true })
    expect(shown(w)).toEqual({ status: false, cancels: false, periodEnd: false, manage: true })
  })

  it('shows an ended subscription no status, with Manage Subscription for its invoices', async () => {
    // #4432: the org is back on Free, so `canceled` is no subscription it has.
    const w = await mountWith({ ...free, status: 'canceled', has_billing_account: true })
    expect(shown(w)).toEqual({ status: false, cancels: false, periodEnd: false, manage: true })
  })
})

// #4432 — a checkout abandoned at 3-D Secure leaves the org on Free with an
// `incomplete` subscription, as an ended one leaves it `canceled`: neither is
// a subscription the org has. The chip shows only while the summary says
// `subscribed` (active, trialing or past due).
describe('the status chip (#4432)', () => {
  const chip = (w: Awaited<ReturnType<typeof mountWith>>) => {
    const c = w.find('[data-testid="billing-status"]')
    if (!c.exists()) return null
    const classes = c.classes()
    return { text: c.text(), success: classes.includes('text-success'), warning: classes.includes('text-warning') }
  }
  const subscribed = { ...cancelling, cancel_at_period_end: false }

  it('is hidden while not subscribed, incomplete after an abandoned 3-D Secure checkout', async () => {
    const w = await mountWith({ ...free, status: 'incomplete', has_billing_account: true })
    expect(chip(w)).toBeNull()
  })

  it('is hidden while not subscribed, active as the start of a checkout records it', async () => {
    const w = await mountWith({ ...free, status: 'active', has_billing_account: true })
    expect(chip(w)).toBeNull()
  })

  it('warns while subscribed and past due', async () => {
    const w = await mountWith({ ...subscribed, status: 'past_due' })
    expect(chip(w)).toEqual({ text: 'past_due', success: false, warning: true })
  })

  it('shows success while subscribed and active', async () => {
    const w = await mountWith(subscribed)
    expect(chip(w)).toEqual({ text: 'active', success: true, warning: false })
  })
})
