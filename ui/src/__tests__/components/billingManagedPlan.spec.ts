// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4433 — an org whose plan Roomler set by hand (Enterprise, with no
// subscription) was offered "Upgrade to Pro" and "Upgrade to Business", and
// paying would have replaced its plan with the one bought. The summary's
// `plan_managed` says the plan was set outside billing: the page then offers
// no checkout below it and says how the plan is changed. The server refuses
// such a checkout anyway (409 `plan_managed`), and a page that still offered
// one says why.
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
const SUMMARY_URL = `/stripe/summary?tenant_id=${TENANT}`
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
// What `GET /api/stripe/plans` lists: never Enterprise.
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
  plan_managed: false,
}
/** An org on `plan`, set by hand: no subscription, no billing. */
const managed = (plan: string) => ({ ...free, plan, plan_managed: true })

/** Answer the page's reads: the plans, and the org's summary from `summary()`. */
function answerWith(summary: () => object) {
  vi.mocked(api.get).mockImplementation(async (url: string) => {
    if (url === '/stripe/plans') return plans as never
    if (url === SUMMARY_URL) return summary() as never
    throw new Error(`unexpected ${url}`)
  })
}

async function mountWith(summary: object) {
  answerWith(() => summary)
  useTenantStore().current = { id: TENANT, name: 'Org', slug: 'org', plan: 'free' }
  const w = mount(BillingView, {
    global: { plugins: [createVuetify({ components, directives })], mocks: { $route: { query: {} } } },
  })
  await flushPromises()
  return w
}

type Page = Awaited<ReturnType<typeof mountWith>>
const buttonsOf = (w: Page) => w.findAll('button').map((b) => b.text().trim())
const notice = (w: Page) => w.find('[data-testid="billing-managed"]')

beforeEach(() => {
  setActivePinia(createPinia())
  vi.clearAllMocks()
})

describe('a plan set outside billing (#4433)', () => {
  it('offers an Enterprise org no checkout, and says its plan was set by Roomler', async () => {
    const w = await mountWith(managed('enterprise'))

    expect(buttonsOf(w)).not.toContain('Upgrade to Pro')
    expect(buttonsOf(w)).not.toContain('Upgrade to Business')
    expect(notice(w).exists()).toBe(true)
    expect(notice(w).text()).toContain('set by Roomler')
    expect(notice(w).text()).toContain('contact')
  })

  it('offers a Business org nothing below its plan', async () => {
    const w = await mountWith(managed('business'))

    expect(buttonsOf(w)).not.toContain('Upgrade to Pro')
    expect(w.get('[data-testid="plan-card-business"]').text()).toContain('Current Plan')
    expect(notice(w).exists()).toBe(true)
  })

  it('still offers a granted Pro org Business, which raises its plan', async () => {
    const w = await mountWith(managed('pro'))

    expect(buttonsOf(w)).toContain('Upgrade to Business')
    expect(w.get('[data-testid="plan-card-pro"]').text()).toContain('Current Plan')
  })

  it('still offers a Free org both upgrades, with no notice', async () => {
    const w = await mountWith(free)

    expect(buttonsOf(w)).toContain('Upgrade to Pro')
    expect(buttonsOf(w)).toContain('Upgrade to Business')
    expect(notice(w).exists()).toBe(false)
  })

  it('says why when the server refuses a checkout as managed, and reads the org again', async () => {
    // The page was loaded before the plan was granted, so it still offers Pro.
    const w = await mountWith(free)
    vi.mocked(api.post).mockRejectedValue(
      Object.assign(new Error('refused'), { status: 409, data: { error: 'plan_managed', message: 'refused' } }),
    )
    answerWith(() => managed('enterprise'))

    await clickUpgradeToPro(w)

    // The page's own words, chosen by the code: never a bare "Checkout failed".
    const alert = w.get('[data-testid="billing-error"]').text()
    expect(alert).toContain('set by Roomler')
    expect(alert).not.toContain('Checkout failed')
    const summaryReads = vi.mocked(api.get).mock.calls.filter(([url]) => url === SUMMARY_URL)
    expect(summaryReads).toHaveLength(2)
    expect(buttonsOf(w)).not.toContain('Upgrade to Pro')
    expect(notice(w).exists()).toBe(true)
  })

  it('keeps any other refused checkout as it was', async () => {
    const w = await mountWith(free)
    const words = 'This organization already has a subscription — change the plan instead'
    vi.mocked(api.post).mockRejectedValue(
      Object.assign(new Error(words), { status: 409, data: { error: 'conflict', message: words } }),
    )

    await clickUpgradeToPro(w)

    expect(w.get('[data-testid="billing-error"]').text()).toContain(`Checkout failed: ${words}`)
    expect(notice(w).exists()).toBe(false)
  })
})

async function clickUpgradeToPro(w: Page) {
  const upgrade = w.findAll('button').find((b) => b.text().trim() === 'Upgrade to Pro')
  expect(upgrade).toBeDefined()
  await upgrade!.trigger('click')
  await flushPromises()
}
