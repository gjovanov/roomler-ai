// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3029 — the page the activation email's link lands on: it redeems the
// token on arrival and hands the person to sign-in; a 400 and a half link
// say the link is invalid, with the ways back; a refresh after success
// shows the success again without a second POST; two mounts make one POST.
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
    expect(w.find('[data-test="activate-register"]').exists()).toBe(false)
  })

  it('a 400 says the link is invalid or expired, with sign-in and register as the ways back', async () => {
    answer(400)
    const { w } = await land(LINK)
    expect(w.find('[data-test="activate-invalid"]').text()).toBe('This activation link is invalid or has expired')
    expect(w.text()).toContain('already activated your account, just sign in')
    expect(w.find('[data-test="activate-sign-in"]').attributes('href')).toBe('/login')
    expect(w.find('[data-test="activate-register"]').attributes('href')).toBe('/register')
  })

  it('a link missing a parameter gets the same words, without calling the server', async () => {
    answer(200)
    const half = await land(`/auth/activate?userId=${USER_ID}`)
    expect(fetchMock).not.toHaveBeenCalled()
    expect(half.w.find('[data-test="activate-invalid"]').exists()).toBe(true)
    half.w.unmount()
    const bare = await land('/auth/activate')
    expect(fetchMock).not.toHaveBeenCalled()
    expect(bare.w.find('[data-test="activate-invalid"]').exists()).toBe(true)
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
