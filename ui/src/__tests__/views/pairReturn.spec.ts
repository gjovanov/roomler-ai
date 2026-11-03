// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-129 (#3022) A2 — the return to `/pair`. A person the pairing page sent
// to sign in or sign up comes back to it, in the tab they left from
// (`pending_redirect`) or in the new tab an email activation opens
// (`pending_pair_until`, localStorage). Each place that decides "signed in —
// where to?" honours the marker: the sign-in page, the OAuth callback and
// the sign-up page (the router guard's branch is routerPair.spec.ts).
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createMemoryHistory, createRouter } from 'vue-router'
import { createPinia } from 'pinia'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))
vi.mock('@/plugins/router', () => ({ default: { push: vi.fn() } }))
vi.mock('@/composables/usePush', () => ({
  subscribePush: vi.fn(() => Promise.resolve()),
  unsubscribePush: vi.fn(() => Promise.resolve()),
}))
vi.mock('@/stores/ws', () => ({ useWsStore: () => ({ connect: vi.fn() }) }))
vi.mock('@/utils/attribution', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/utils/attribution')>()),
  startOAuth: vi.fn(),
}))

import LoginView from '@/views/auth/LoginView.vue'
import RegisterView from '@/views/auth/RegisterView.vue'
import OAuthCallbackView from '@/views/auth/OAuthCallbackView.vue'
import { api } from '@/api/client'
import { PENDING_PAIR_KEY, PENDING_PAIR_TTL_MS, rememberPendingPair } from '@/utils/pairing'

const vuetify = createVuetify({ components, directives })
const mockApi = vi.mocked(api)
const Stub = { template: '<div />' }
const USER = { id: 'u1', email: 'a@example.com', username: 'a', display_name: 'A' }

function makeRouter(path: string, view: unknown) {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path, component: view as never },
      { path: '/login', name: 'login', component: path === '/login' ? (view as never) : Stub },
      { path: '/register', name: 'register', component: path === '/register' ? (view as never) : Stub },
      { path: '/', name: 'dashboard', component: Stub },
      { path: '/pair', name: 'pair', component: Stub },
      { path: '/invite/:code', name: 'invite', component: Stub },
      { path: '/tenant/:tenantId', name: 'tenant-dashboard', component: Stub },
    ],
  })
}

async function mountView(path: string, view: unknown) {
  const router = makeRouter(path, view)
  await router.push(path)
  await router.isReady()
  const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
  const w = mount(view as never, {
    global: { plugins: [vuetify, router, createPinia(), i18n] },
    attachTo: document.body,
  })
  await flushPromises()
  return { w, router }
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
  mockApi.post.mockReset()
  mockApi.get.mockReset()
  localStorage.clear()
  sessionStorage.clear()
})

afterEach(() => {
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
  document.body.innerHTML = ''
  window.history.replaceState({}, '', '/')
})

describe('the sign-in page', () => {
  async function signIn() {
    mockApi.post.mockResolvedValue({ user: USER })
    const m = await mountView('/login', LoginView)
    await m.w.find('input[autocomplete="username"]').setValue('a')
    await m.w.find('input[type="password"]').setValue('secret')
    await m.w.find('form').trigger('submit')
    await flushPromises()
    return m
  }

  it('comes back to /pair while a pairing waits, and spends the marker', async () => {
    rememberPendingPair()
    const { router } = await signIn()
    await vi.waitFor(() => expect(router.currentRoute.value.path).toBe('/pair'))
    expect(localStorage.getItem(PENDING_PAIR_KEY)).toBeNull()
  })

  it('goes to the dashboard once the marker has expired, and clears it', async () => {
    rememberPendingPair(Date.now() - PENDING_PAIR_TTL_MS - 1)
    const { router } = await signIn()
    await vi.waitFor(() => expect(router.currentRoute.value.name).toBe('dashboard'))
    expect(localStorage.getItem(PENDING_PAIR_KEY)).toBeNull()
  })

  it('goes to the dashboard without a marker, as before', async () => {
    const { router } = await signIn()
    await vi.waitFor(() => expect(router.currentRoute.value.name).toBe('dashboard'))
  })

  it('the same-tab return still wins, and the marker stays for the page to spend', async () => {
    rememberPendingPair()
    sessionStorage.setItem('pending_redirect', '/pair')
    const { router } = await signIn()
    await vi.waitFor(() => expect(router.currentRoute.value.path).toBe('/pair'))
    expect(sessionStorage.getItem('pending_redirect')).toBeNull()
    expect(localStorage.getItem(PENDING_PAIR_KEY)).not.toBeNull()
  })
})

describe('the OAuth callback', () => {
  async function land() {
    window.history.replaceState({}, '', '/oauth/callback')
    vi.stubGlobal('fetch', vi.fn(() => Promise.resolve({ ok: true })))
    mockApi.get.mockResolvedValue(USER)
    return mountView('/oauth/callback', OAuthCallbackView)
  }

  it('comes back to /pair while a pairing waits, and spends the marker', async () => {
    rememberPendingPair()
    const { router } = await land()
    await vi.waitFor(() => expect(router.currentRoute.value.path).toBe('/pair'))
    expect(localStorage.getItem(PENDING_PAIR_KEY)).toBeNull()
  })

  it('goes to the dashboard without a marker', async () => {
    const { router } = await land()
    await vi.waitFor(() => expect(router.currentRoute.value.name).toBe('dashboard'))
  })
})

describe('the sign-up page', () => {
  async function fill(w: Awaited<ReturnType<typeof mountView>>['w']) {
    const inputs = w.findAll('input')
    await inputs[0]!.setValue('new@example.com')
    await inputs[1]!.setValue('newuser')
    await inputs[2]!.setValue('New User')
    await w.find('input[type="password"]').setValue('secret123')
  }
  function registerBody(): Record<string, unknown> {
    const call = mockApi.post.mock.calls.find(([url]) => url === '/auth/register')
    expect(call, 'no register request was sent').toBeDefined()
    return call![1] as Record<string, unknown>
  }

  it('offers the organization only while a pairing waits', async () => {
    const plain = await mountView('/register', RegisterView)
    expect(plain.w.find('[data-test="register-org"]').exists()).toBe(false)
    plain.w.unmount()
    document.body.innerHTML = ''

    rememberPendingPair()
    const { w } = await mountView('/register', RegisterView)
    expect(w.find('[data-test="register-org"]').exists()).toBe(true)
    expect(w.text()).toContain('The device you are pairing joins an organization')
  })

  it('sends the organization with the account, the slug following the name', async () => {
    rememberPendingPair()
    mockApi.post.mockResolvedValue({ message: 'Registration successful. Please check your email to activate your account.' })
    const { w, router } = await mountView('/register', RegisterView)
    await fill(w)
    await w.find('input[autocomplete="organization"]').setValue('My Home Lab')
    await flushPromises()
    const all = w.findAll('input')
    const slug = all[all.length - 1]!
    expect((slug.element as HTMLInputElement).value).toBe('my-home-lab')
    await w.find('form').trigger('submit')
    await flushPromises()
    expect(registerBody()).toMatchObject({
      email: 'new@example.com',
      username: 'newuser',
      display_name: 'New User',
      tenant_name: 'My Home Lab',
      tenant_slug: 'my-home-lab',
    })
    // No session on production: stay, say "check your email", and say the
    // sign-in afterwards leads back to the pairing.
    expect(router.currentRoute.value.path).toBe('/register')
    const note = w.find('[data-test="register-activation"]')
    expect(note.text()).toContain('check your email')
    expect(note.text()).toContain('brought back to the pairing page')
    // The marker is for the tab that signs in later, so it stays.
    expect(localStorage.getItem(PENDING_PAIR_KEY)).not.toBeNull()
  })

  it('sends no organization when the fields are left empty', async () => {
    rememberPendingPair()
    mockApi.post.mockResolvedValue({ message: 'check your email' })
    const { w } = await mountView('/register', RegisterView)
    await fill(w)
    await w.find('form').trigger('submit')
    await flushPromises()
    expect(registerBody()).not.toHaveProperty('tenant_name')
    expect(registerBody()).not.toHaveProperty('tenant_slug')
  })

  it('sends none without a marker either, and leaves for the dashboard as before', async () => {
    mockApi.post.mockResolvedValue({ message: 'check your email' })
    const { w, router } = await mountView('/register', RegisterView)
    await fill(w)
    await w.find('form').trigger('submit')
    await flushPromises()
    expect(registerBody()).not.toHaveProperty('tenant_name')
    await vi.waitFor(() => expect(router.currentRoute.value.name).toBe('dashboard'))
  })

  it('an account born with a session goes straight back to /pair', async () => {
    rememberPendingPair()
    mockApi.post.mockResolvedValue({ user: USER })
    const { w, router } = await mountView('/register', RegisterView)
    await fill(w)
    await w.find('form').trigger('submit')
    await flushPromises()
    await vi.waitFor(() => expect(router.currentRoute.value.path).toBe('/pair'))
    expect(localStorage.getItem(PENDING_PAIR_KEY)).toBeNull()
  })
})
