// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3029 — the page the activation email's link lands on: it redeems the
// token on arrival and hands the person to sign-in; a 400 and a half link
// say the link is invalid, with the ways back; a refresh after success
// shows the success again without a second POST; two mounts make one POST.
// #3199 — the way back from a refused link is a new link, asked for here, and
// the page says the same words whatever the address.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createMemoryHistory, createRouter } from 'vue-router'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'
import ActivateView from '@/views/auth/ActivateView.vue'

const vuetify = createVuetify({ components, directives })
const Stub = { template: '<div />' }
const USER_ID = 'bbbbbbbbbbbbbbbbbbbbbbbb'
const LINK = `/auth/activate?userId=${USER_ID}&token=abc1234`

let fetchMock: ReturnType<typeof vi.fn>

function answer(status: number) {
  fetchMock = vi.fn(async () => ({ ok: status >= 200 && status < 300, status, json: async () => ({}) }))
  vi.stubGlobal('fetch', fetchMock)
}

async function askForALink(w: Awaited<ReturnType<typeof land>>['w'], address: string) {
  await w.find('[data-test="activate-email"] input').setValue(address)
  await w.find('[data-test="activate-resend"]').trigger('submit')
  await flushPromises()
}

function lastCall(): [string, RequestInit] {
  return fetchMock.mock.calls.at(-1) as unknown as [string, RequestInit]
}

async function land(url: string) {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/auth/activate', name: 'activate', component: ActivateView },
      { path: '/login', name: 'login', component: Stub },
      { path: '/register', name: 'register', component: Stub },
    ],
  })
  await router.push(url)
  await router.isReady()
  const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
  const w = mount(ActivateView, { global: { plugins: [vuetify, router, i18n] }, attachTo: document.body })
  await flushPromises()
  return { w, router }
}

function sentBody(): Record<string, unknown> {
  const call = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
  return JSON.parse(String(call[1].body)) as Record<string, unknown>
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
  sessionStorage.clear()
})

afterEach(() => {
  vi.unstubAllGlobals()
  document.body.innerHTML = ''
  sessionStorage.clear()
})

describe('ActivateView', () => {
  it('redeems the token on arrival with the server’s shape, then offers sign-in', async () => {
    answer(200)
    const { w } = await land(LINK)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    expect((fetchMock.mock.calls[0] as unknown as [string])[0]).toBe('/api/auth/activate')
    expect(sentBody()).toEqual({ user_id: USER_ID, token: 'abc1234' })
    expect(w.find('[data-test="activate-done"]').text()).toBe('Your account is active')
    const signIn = w.find('[data-test="activate-sign-in"]')
    expect(signIn.exists()).toBe(true)
    expect(signIn.attributes('href')).toBe('/login')
    expect(w.find('[data-test="activate-resend"]').exists()).toBe(false)
  })

  it('a 400 says the link is invalid or expired, and offers a new link and sign-in', async () => {
    answer(400)
    const { w } = await land(LINK)
    expect(w.find('[data-test="activate-invalid"]').text()).toBe('This activation link is invalid or has expired')
    expect(w.text()).toContain('already active, just sign in')
    expect(w.find('[data-test="activate-sign-in"]').attributes('href')).toBe('/login')
    expect(w.find('[data-test="activate-resend"]').exists()).toBe(true)
  })

  it('never sends a refused link back to registering: that address answers 409 (#3199)', async () => {
    answer(400)
    const { w } = await land(LINK)
    expect(w.text().toLowerCase()).not.toContain('register')
    expect(w.find('a[href="/register"]').exists()).toBe(false)
  })

  it('a link missing a parameter gets the same words, without calling the server', async () => {
    answer(200)
    const half = await land(`/auth/activate?userId=${USER_ID}`)
    expect(fetchMock).not.toHaveBeenCalled()
    expect(half.w.find('[data-test="activate-invalid"]').exists()).toBe(true)
  })

  it('with no link at all it just asks: that is sign-in’s and sign-up’s way in', async () => {
    answer(200)
    const { w } = await land('/auth/activate')
    expect(fetchMock).not.toHaveBeenCalled()
    expect(w.find('[data-test="activate-invalid"]').exists()).toBe(false)
    expect(w.find('[data-test="activate-ask"]').text()).toBe('Get a new activation link')
    expect(w.find('[data-test="activate-resend"]').exists()).toBe(true)
  })

  it('asks for a new link with the server’s shape, the address trimmed', async () => {
    answer(400)
    const { w } = await land(LINK)
    answer(202)
    await askForALink(w, '  person@example.com ')
    expect(fetchMock).toHaveBeenCalledTimes(1)
    const [url, init] = lastCall()
    expect(url).toBe('/api/auth/resend-activation')
    expect(init.method).toBe('POST')
    expect(JSON.parse(String(init.body))).toEqual({ email: 'person@example.com' })
  })

  it('says one thing on a 202, and never that the address has an account', async () => {
    answer(202)
    const { w } = await land('/auth/activate')
    await askForALink(w, 'person@example.com')
    const words = w.find('[data-test="activate-resend-outcome"]').text()
    expect(words).toBe(en.activation.resend.sent)
    expect(words).toContain('If that address is waiting for activation')
    expect(words).not.toContain('person@example.com')
  })

  it('does not ask without an address', async () => {
    answer(202)
    const { w } = await land('/auth/activate')
    expect(w.find('[data-test="activate-resend-send"]').attributes('disabled')).toBeDefined()
    await askForALink(w, '   ')
    expect(fetchMock).not.toHaveBeenCalled()
    expect(w.find('[data-test="activate-resend-outcome"]').exists()).toBe(false)
  })

  it('a 429 says to wait, and an unreachable server says to try again', async () => {
    answer(429)
    const { w } = await land('/auth/activate')
    await askForALink(w, 'person@example.com')
    expect(w.find('[data-test="activate-resend-outcome"]').text()).toBe(en.activation.resend.throttled)
    fetchMock = vi.fn(async () => Promise.reject(new TypeError('Failed to fetch')))
    vi.stubGlobal('fetch', fetchMock)
    await askForALink(w, 'person@example.com')
    expect(w.find('[data-test="activate-resend-outcome"]').text()).toBe(en.activation.resend.error)
  })

  it('a refresh after success shows the success again, with no second POST', async () => {
    answer(200)
    const first = await land(LINK)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    first.w.unmount()
    // The same tab, the same link, loaded again: the token is spent, the
    // account is active — the page must not turn that into "invalid".
    const again = await land(LINK)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    expect(again.w.find('[data-test="activate-done"]').exists()).toBe(true)
  })

  it('two mounts at once make one POST (a double-fired effect)', async () => {
    let release!: (v: unknown) => void
    fetchMock = vi.fn(() => new Promise((r) => (release = r)))
    vi.stubGlobal('fetch', fetchMock)
    const router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/auth/activate', component: ActivateView }, { path: '/login', component: Stub }, { path: '/register', component: Stub }],
    })
    await router.push(LINK)
    await router.isReady()
    const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
    const a = mount(ActivateView, { global: { plugins: [vuetify, router, i18n] } })
    const b = mount(ActivateView, { global: { plugins: [vuetify, router, i18n] } })
    await flushPromises()
    expect(fetchMock).toHaveBeenCalledTimes(1)
    expect(a.find('[data-test="activate-working"]').exists()).toBe(true)
    release({ ok: true, status: 200 })
    await flushPromises()
    expect(a.find('[data-test="activate-done"]').exists()).toBe(true)
    expect(b.find('[data-test="activate-done"]').exists()).toBe(true)
    a.unmount()
    b.unmount()
  })

  it('an unreachable server is said as such, and "Try again" asks again', async () => {
    fetchMock = vi.fn(async () => Promise.reject(new TypeError('Failed to fetch')))
    vi.stubGlobal('fetch', fetchMock)
    const { w } = await land(LINK)
    expect(w.find('[data-test="activate-error"]').text()).toBe('Could not reach the server')
    fetchMock.mockImplementation(async () => ({ ok: true, status: 200, json: async () => ({}) }))
    await w.find('[data-test="activate-retry"]').trigger('click')
    await flushPromises()
    expect(fetchMock).toHaveBeenCalledTimes(2)
    expect(w.find('[data-test="activate-done"]').exists()).toBe(true)
  })

  it('a 5xx is "could not reach", not "invalid": nothing is known about the link', async () => {
    answer(503)
    const { w } = await land(LINK)
    expect(w.find('[data-test="activate-error"]').exists()).toBe(true)
    expect(w.find('[data-test="activate-invalid"]').exists()).toBe(false)
  })
})
