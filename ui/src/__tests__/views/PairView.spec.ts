// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-129 (#3022) A2 — the `/pair` page with the API mocked: signed out (and
// every way in leaves both returns), the code input, a lookup's 404 and 429,
// the device card and its warning, the organization picker and the inline
// create, an approve's 403 inline, and the approved, denied and gone screens.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createMemoryHistory, createRouter } from 'vue-router'
import { createPinia } from 'pinia'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'

vi.mock('@/api/client', () => {
  class ApiError extends Error {
    constructor(
      public status: number,
      public data: unknown,
      message?: string,
    ) {
      super(message || (data as Record<string, string> | null)?.message || `API error ${status}`)
    }
  }
  return { api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() }, ApiError }
})
// The auth store imports the app router; nothing here navigates through it.
vi.mock('@/plugins/router', () => ({ default: { push: vi.fn() } }))
vi.mock('@/composables/usePush', () => ({
  subscribePush: vi.fn(() => Promise.resolve()),
  unsubscribePush: vi.fn(() => Promise.resolve()),
}))
vi.mock('@/utils/attribution', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/utils/attribution')>()),
  startOAuth: vi.fn(),
}))

import PairView from '@/views/pair/PairView.vue'
import { api, ApiError } from '@/api/client'
import { startOAuth } from '@/utils/attribution'
import { PENDING_PAIR_KEY, PENDING_PAIR_TTL_MS, rememberPendingPair } from '@/utils/pairing'

const vuetify = createVuetify({ components, directives })
const mockApi = vi.mocked(api)
const mockStartOAuth = vi.mocked(startOAuth)
const Stub = { template: '<div />' }

const ORGS = [
  { id: 't1', name: 'Acme', slug: 'acme' },
  { id: 't2', name: 'Beta Labs', slug: 'beta-labs' },
]

const DEVICE = {
  machine_name: 'kitchen-pc',
  os: 'windows',
  arch: 'x86_64',
  role: 'daemon',
  halves: 1,
  client: 'roomler-setup',
  client_version: '0.4.180',
  country: 'DE',
  started_at: new Date(Date.now() - 2 * 60_000).toISOString(),
  expires_at: new Date(Date.now() + 13 * 60_000).toISOString(),
}

async function mountPair(opts: { signedIn: boolean; tenants?: unknown[] } = { signedIn: true }) {
  if (opts.signedIn) localStorage.setItem('roomler-signed-in', '1')
  else localStorage.removeItem('roomler-signed-in')
  mockApi.get.mockImplementation(async (path: string) => {
    if (path === '/tenant') return opts.tenants ?? ORGS
    return undefined
  })
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/pair', name: 'pair', component: PairView },
      { path: '/login', name: 'login', component: Stub },
      { path: '/register', name: 'register', component: Stub },
      { path: '/', name: 'dashboard', component: Stub },
    ],
  })
  await router.push('/pair')
  await router.isReady()
  const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
  const w = mount(PairView, {
    global: { plugins: [vuetify, router, createPinia(), i18n] },
    attachTo: document.body,
  })
  await flushPromises()
  return { w, router }
}

type Mounted = Awaited<ReturnType<typeof mountPair>>

async function typeCode(w: Mounted['w'], raw: string) {
  const input = w.find('[data-test="pair-code"] input')
  expect(input.exists()).toBe(true)
  await input.setValue(raw)
  await flushPromises()
  return input
}

async function lookup(w: Mounted['w'], raw = 'bcdf-ghjk') {
  await typeCode(w, raw)
  await w.find('form').trigger('submit')
  await flushPromises()
}

function postCalls(path: string) {
  return mockApi.post.mock.calls.filter(([p]) => p === path)
}

beforeEach(() => {
  vi.stubGlobal(
    'ResizeObserver',
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  )
  mockApi.get.mockReset()
  mockApi.post.mockReset()
  mockStartOAuth.mockReset()
  localStorage.removeItem(PENDING_PAIR_KEY)
  localStorage.removeItem('roomler-current-tenant')
  sessionStorage.removeItem('pending_redirect')
})

afterEach(() => {
  vi.unstubAllGlobals()
  document.body.innerHTML = ''
  localStorage.clear()
  sessionStorage.clear()
})

describe('PairView — signed out', () => {
  it('explains itself, and offers sign-in, sign-up and the providers', async () => {
    const { w } = await mountPair({ signedIn: false })
    expect(w.find('[data-test="pair-signed-out"]').text()).toContain('connects the installer running on your computer to your account')
    expect(w.find('[data-test="pair-sign-in"]').exists()).toBe(true)
    expect(w.find('[data-test="pair-register"]').exists()).toBe(true)
    expect(w.findAll('button').map((b) => b.text().trim())).toEqual(
      expect.arrayContaining(['Google', 'Facebook', 'GitHub', 'LinkedIn', 'Microsoft']),
    )
    // No code input for a stranger: nothing to look up before signing in.
    expect(w.find('[data-test="pair-code"]').exists()).toBe(false)
    expect(mockApi.get).not.toHaveBeenCalledWith('/tenant')
  })

  it('"Sign in" leaves both returns and goes to the sign-in page', async () => {
    const { w, router } = await mountPair({ signedIn: false })
    const before = Date.now()
    await w.find('[data-test="pair-sign-in"]').trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/login')
    expect(sessionStorage.getItem('pending_redirect')).toBe('/pair')
    const until = Number(localStorage.getItem(PENDING_PAIR_KEY))
    expect(until).toBeGreaterThanOrEqual(before + PENDING_PAIR_TTL_MS)
    expect(until).toBeLessThanOrEqual(Date.now() + PENDING_PAIR_TTL_MS)
  })

  it('"Create an account" does the same, towards the sign-up page', async () => {
    const { w, router } = await mountPair({ signedIn: false })
    await w.find('[data-test="pair-register"]').trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/register')
    expect(sessionStorage.getItem('pending_redirect')).toBe('/pair')
    expect(localStorage.getItem(PENDING_PAIR_KEY)).not.toBeNull()
  })

  it('a provider button leaves both returns and starts the provider sign-in', async () => {
    const { w } = await mountPair({ signedIn: false })
    const google = w.findAll('button').find((b) => b.text().trim() === 'Google')
    await google!.trigger('click')
    expect(mockStartOAuth).toHaveBeenCalledTimes(1)
    expect(mockStartOAuth.mock.calls[0]![0]).toBe('google')
    expect(sessionStorage.getItem('pending_redirect')).toBe('/pair')
    expect(localStorage.getItem(PENDING_PAIR_KEY)).not.toBeNull()
  })

  it('still works when storage refuses: the buttons navigate, nothing throws', async () => {
    const { w, router } = await mountPair({ signedIn: false })
    const setItem = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new DOMException('SecurityError')
    })
    try {
      await w.find('[data-test="pair-sign-in"]').trigger('click')
      await flushPromises()
      expect(router.currentRoute.value.path).toBe('/login')
    } finally {
      setItem.mockRestore()
    }
  })
})

describe('PairView — the code', () => {
  it('spends the marker that brought a signed-in person here', async () => {
    rememberPendingPair()
    await mountPair({ signedIn: true })
    expect(localStorage.getItem(PENDING_PAIR_KEY)).toBeNull()
  })

  it('upper-cases, drops what is not a code and inserts the hyphen as the person types', async () => {
    const { w } = await mountPair()
    const input = await typeCode(w, 'bcd')
    expect((input.element as HTMLInputElement).value).toBe('BCD')
    await typeCode(w, 'bcdfg')
    expect((input.element as HTMLInputElement).value).toBe('BCDF-G')
    await typeCode(w, 'bcdf ghj1k')
    expect((input.element as HTMLInputElement).value).toBe('BCDF-GHJK')
    // The keyboard is asked for capitals and no corrections.
    expect(input.attributes('autocapitalize')).toBe('characters')
    expect(input.attributes('autocomplete')).toBe('off')
  })

  it('looks up only a complete code, sending the eight letters', async () => {
    const { w } = await mountPair()
    await typeCode(w, 'bcdf-gh')
    expect((w.find('[data-test="pair-lookup"]').element as HTMLButtonElement).disabled).toBe(true)
    await w.find('form').trigger('submit')
    await flushPromises()
    expect(postCalls('/pair/lookup')).toHaveLength(0)

    mockApi.post.mockResolvedValueOnce(DEVICE)
    await lookup(w)
    expect(postCalls('/pair/lookup')).toEqual([['/pair/lookup', { user_code: 'BCDFGHJK' }]])
  })

  it('says, neutrally, that no pairing is pending on a 404 — and names the paste fallback', async () => {
    const { w } = await mountPair()
    mockApi.post.mockRejectedValueOnce(new ApiError(404, { error: 'not_found', message: 'no such pairing' }))
    await lookup(w)
    const err = w.find('[data-test="pair-lookup-error"]')
    expect(err.text()).toContain('No pending pairing with that code')
    expect(err.text()).toContain('paste a token into your installer')
    expect(err.text()).not.toContain('no such pairing')
    // Still on the code step, with the code kept for a correction.
    expect(w.find('[data-test="pair-code"]').exists()).toBe(true)
  })

  it('shows the rate-limit text on a 429, as the api client phrased it', async () => {
    const { w } = await mountPair()
    mockApi.post.mockRejectedValueOnce(new ApiError(429, {}, 'Too many requests. Try again in 12s.'))
    await lookup(w)
    expect(w.find('[data-test="pair-lookup-error"]').text()).toBe('Too many requests. Try again in 12s.')
  })

  it('shows the server’s words on any other refusal', async () => {
    const { w } = await mountPair()
    mockApi.post.mockRejectedValueOnce(new ApiError(503, { error: 'unavailable', message: 'Pairing is unavailable right now' }))
    await lookup(w)
    expect(w.find('[data-test="pair-lookup-error"]').text()).toBe('Pairing is unavailable right now')
  })
})

describe('PairView — the device', () => {
  async function review(tenants: unknown[] = ORGS, device: Record<string, unknown> = DEVICE) {
    const m = await mountPair({ signedIn: true, tenants })
    mockApi.post.mockResolvedValueOnce(device)
    await lookup(m.w)
    expect(m.w.find('[data-test="pair-device"]').exists()).toBe(true)
    return m
  }

  it('shows exactly what is being approved before Approve', async () => {
    const { w } = await review()
    const card = w.find('[data-test="pair-device"]').text()
    expect(card).toContain('kitchen-pc')
    expect(card).toContain('Windows · x86-64')
    expect(card).toContain("This computer's remote desktop and private network")
    expect(card).toContain('roomler-setup 0.4.180')
    expect(card).toContain('Germany')
    expect(card).toContain('2 minutes ago')
    expect(w.find('[data-test="pair-halves"]').exists()).toBe(false)
    const warning = w.find('[data-test="pair-warning"]')
    expect(warning.text()).toContain('Only approve a device you are setting up right now, in front of you.')
    // The code step is gone; the decision is here.
    expect(w.find('[data-test="pair-code"]').exists()).toBe(false)
    expect(w.find('[data-test="pair-approve"]').exists()).toBe(true)
    expect(w.find('[data-test="pair-deny"]').exists()).toBe(true)
  })

  it('a tunnel client, a Mac in two halves, an unknown country, a start just now', async () => {
    const { w } = await review(ORGS, {
      ...DEVICE,
      os: 'macos',
      arch: 'aarch64',
      role: 'tunnel',
      halves: 2,
      country: null,
      client: 'roomler',
      client_version: '0.4.181',
      started_at: new Date().toISOString(),
    })
    const card = w.find('[data-test="pair-device"]').text()
    expect(card).toContain('macOS · ARM64')
    expect(card).toContain('A tunnel client')
    expect(card).toContain('roomler 0.4.181')
    expect(card).toContain('an unknown location')
    expect(card).toContain('just now')
    expect(w.find('[data-test="pair-halves"]').text()).toContain('two entries')
  })

  it('offers the person’s organizations, the current one first', async () => {
    localStorage.setItem('roomler-current-tenant', 't2')
    const { w } = await review()
    const picker = w.find('[data-test="pair-org"]')
    expect(picker.exists()).toBe(true)
    expect(picker.text()).toContain('Beta Labs')
    expect(w.find('[data-test="pair-no-orgs"]').exists()).toBe(false)
  })

  it('approves into the chosen organization, then says the installer continues', async () => {
    const { w } = await review()
    mockApi.post.mockResolvedValueOnce({ approved: true })
    await w.find('[data-test="pair-approve"]').trigger('click')
    await flushPromises()
    expect(postCalls('/pair/approve')).toEqual([['/pair/approve', { user_code: 'BCDFGHJK', tenant_id: 't1' }]])
    expect(w.find('[data-test="pair-approved"]').text()).toBe(
      'Approved — your installer continues on its own. You can close this tab.',
    )
    expect(w.find('[data-test="pair-approve"]').exists()).toBe(false)
  })

  it('denies, then says the installer stops', async () => {
    const { w } = await review()
    mockApi.post.mockResolvedValueOnce({ denied: true })
    await w.find('[data-test="pair-deny"]').trigger('click')
    await flushPromises()
    expect(postCalls('/pair/deny')).toEqual([['/pair/deny', { user_code: 'BCDFGHJK' }]])
    expect(w.find('[data-test="pair-denied"]').text()).toBe('Denied — the installer will stop.')
    expect(postCalls('/pair/approve')).toHaveLength(0)
  })

  it('a 403 at approve stays on the page, in words, with the server’s detail — never a sign-out', async () => {
    const { w, router } = await review()
    mockApi.post.mockRejectedValueOnce(
      new ApiError(403, { error: 'forbidden', message: 'Missing permission: MANAGE_AGENTS' }),
    )
    await w.find('[data-test="pair-approve"]').trigger('click')
    await flushPromises()
    const err = w.find('[data-test="pair-decide-error"]')
    expect(err.text()).toContain('Manage devices permission')
    expect(err.text()).toContain('Missing permission: MANAGE_AGENTS')
    expect(w.find('[data-test="pair-approve"]').exists()).toBe(true)
    expect(w.find('[data-test="pair-org"]').exists()).toBe(true)
    expect(router.currentRoute.value.path).toBe('/pair')
    expect(localStorage.getItem('roomler-signed-in')).toBe('1')
  })

  it('a 403 for a tunnel client says it is about membership', async () => {
    const { w } = await review(ORGS, { ...DEVICE, role: 'tunnel' })
    mockApi.post.mockRejectedValueOnce(new ApiError(403, { error: 'forbidden' }))
    await w.find('[data-test="pair-approve"]').trigger('click')
    await flushPromises()
    expect(w.find('[data-test="pair-decide-error"]').text()).toContain('not a member of it')
  })

  it('a 404 at approve means expired or decided: say so, and offer another code', async () => {
    const { w } = await review()
    mockApi.post.mockRejectedValueOnce(new ApiError(404, { error: 'not_found' }))
    await w.find('[data-test="pair-approve"]').trigger('click')
    await flushPromises()
    expect(w.find('[data-test="pair-gone"]').text()).toContain('expired or was already decided')
    expect(w.find('[data-test="pair-approve"]').exists()).toBe(false)
    await w.find('[data-test="pair-start-again"]').trigger('click')
    await flushPromises()
    const input = w.find('[data-test="pair-code"] input')
    expect(input.exists()).toBe(true)
    expect((input.element as HTMLInputElement).value).toBe('')
  })

  it('a 404 at deny is the same outcome', async () => {
    const { w } = await review()
    mockApi.post.mockRejectedValueOnce(new ApiError(404, { error: 'not_found' }))
    await w.find('[data-test="pair-deny"]').trigger('click')
    await flushPromises()
    expect(w.find('[data-test="pair-gone"]').exists()).toBe(true)
  })

  it('with no organization, creates one inline and approves into it', async () => {
    const { w } = await review([])
    expect(w.find('[data-test="pair-no-orgs"]').exists()).toBe(true)
    expect(w.find('[data-test="pair-org"]').exists()).toBe(false)
    // Nothing to approve into yet.
    expect((w.find('[data-test="pair-approve"]').element as HTMLButtonElement).disabled).toBe(true)

    const form = w.find('[data-test="pair-create-org"]')
    const inputs = form.findAll('input')
    await inputs[0]!.setValue('My Home Lab')
    await flushPromises()
    expect((inputs[1]!.element as HTMLInputElement).value).toBe('my-home-lab')
    mockApi.post.mockResolvedValueOnce({ id: 't9', name: 'My Home Lab', slug: 'my-home-lab' })
    await form.trigger('submit')
    await flushPromises()
    expect(postCalls('/tenant')).toEqual([['/tenant', { name: 'My Home Lab', slug: 'my-home-lab' }]])
    expect(w.find('[data-test="pair-org"]').text()).toContain('My Home Lab')

    mockApi.post.mockResolvedValueOnce({ approved: true })
    await w.find('[data-test="pair-approve"]').trigger('click')
    await flushPromises()
    expect(postCalls('/pair/approve')).toEqual([['/pair/approve', { user_code: 'BCDFGHJK', tenant_id: 't9' }]])
    expect(w.find('[data-test="pair-approved"]').exists()).toBe(true)
  })

  it('a taken slug is said in words, and the form stays', async () => {
    const { w } = await review([])
    const form = w.find('[data-test="pair-create-org"]')
    await form.findAll('input')[0]!.setValue('Acme')
    await flushPromises()
    mockApi.post.mockRejectedValueOnce(new ApiError(409, { error: 'conflict', message: 'That already exists' }))
    await form.trigger('submit')
    await flushPromises()
    expect(w.find('[data-test="pair-org-error"]').text()).toContain('already taken')
    expect(w.find('[data-test="pair-create-org"]').exists()).toBe(true)
  })

  it('the status says the slug is taken, never the words of the message', async () => {
    const { w } = await review([])
    const form = w.find('[data-test="pair-create-org"]')
    await form.findAll('input')[0]!.setValue('Acme')
    await flushPromises()
    mockApi.post.mockRejectedValueOnce(new ApiError(500, { error: 'internal', message: 'a duplicate write' }))
    await form.trigger('submit')
    await flushPromises()
    expect(w.find('[data-test="pair-org-error"]').text()).toContain('a duplicate write')
  })
})
