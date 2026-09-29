// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-88 (#1790) — the register view: the campaign keys on its URL go out
// ONCE, as `attribution` on the register request or on the OAuth start URL;
// the optional "How did you hear about Roomler?" never blocks sign-up; the
// `signup` goal fires on the server's yes; and nothing is written to the
// device on the way (carry, don't store).
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
// The auth store imports the app router and push; neither matters here.
vi.mock('@/plugins/router', () => ({ default: { push: vi.fn() } }))
vi.mock('@/composables/usePush', () => ({
  subscribePush: vi.fn(() => Promise.resolve()),
  unsubscribePush: vi.fn(() => Promise.resolve()),
}))
vi.mock('@/stores/ws', () => ({ useWsStore: () => ({ connect: vi.fn() }) }))

import RegisterView from '@/views/auth/RegisterView.vue'
import { api } from '@/api/client'

const vuetify = createVuetify({ components, directives })
const mockApi = vi.mocked(api)
type W = Window & { purestat?: unknown }

async function mountAt(path: string) {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/register', name: 'register', component: RegisterView },
      { path: '/', name: 'dashboard', component: { template: '<div />' } },
      { path: '/login', name: 'login', component: { template: '<div />' } },
    ],
  })
  await router.push(path)
  await router.isReady()
  const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
  const w = mount(RegisterView, {
    global: { plugins: [vuetify, router, createPinia(), i18n] },
    attachTo: document.body,
  })
  await flushPromises()
  return { w, router }
}

async function fillAndSubmit(w: Awaited<ReturnType<typeof mountAt>>['w']) {
  const inputs = w.findAll('input')
  await inputs[0]!.setValue('new@example.com')
  await inputs[1]!.setValue('newuser')
  await inputs[2]!.setValue('New User')
  await w.find('input[type="password"]').setValue('secret123')
  await w.find('form').trigger('submit')
  await flushPromises()
}

function registerBody(): Record<string, unknown> {
  const call = mockApi.post.mock.calls.find(([url]) => url === '/auth/register')
  expect(call, 'no register request was sent').toBeDefined()
  return call![1] as Record<string, unknown>
}

const oauthHref = (w: Awaited<ReturnType<typeof mountAt>>['w'], provider: string) =>
  new URL(w.find(`a[href^="/api/oauth/${provider}"]`).attributes('href')!, 'https://roomler.ai')

let setItem: ReturnType<typeof vi.spyOn>
let cookieSet: ReturnType<typeof vi.spyOn>
const purestat = vi.fn()

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
  mockApi.post.mockResolvedValue({ message: 'Registration successful. Please check your email to activate your account.' })
  purestat.mockReset()
  ;(window as W).purestat = purestat
  setItem = vi.spyOn(Storage.prototype, 'setItem')
  cookieSet = vi.spyOn(Document.prototype, 'cookie', 'set')
})

afterEach(() => {
  delete (window as W).purestat
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
  document.body.innerHTML = ''
})

describe('RegisterView — attribution (FR-88)', () => {
  it('sends the landing’s campaign, as the static pages carried it, with the account', async () => {
    const { w } = await mountAt(
      '/register?utm_source=youtube&utm_medium=video&utm_campaign=fr88-test&referrer_host=www.youtube.com&landing_path=%2Fblog%2Fx%2F',
    )
    await fillAndSubmit(w)
    expect(registerBody()).toEqual({
      email: 'new@example.com',
      username: 'newuser',
      password: 'secret123',
      display_name: 'New User',
      attribution: {
        source: 'youtube',
        medium: 'video',
        campaign: 'fr88-test',
        referrer_host: 'www.youtube.com',
        landing_path: '/blog/x/',
      },
    })
  })

  it('works out the landing itself when the visitor arrived here directly', async () => {
    Object.defineProperty(document, 'referrer', { value: 'https://www.tiktok.com/', configurable: true })
    try {
      const { w } = await mountAt('/register?ref=tiktok-bio')
      await fillAndSubmit(w)
      expect(registerBody().attribution).toEqual({ source: 'tiktok-bio', referrer_host: 'www.tiktok.com', landing_path: '/register' })
    } finally {
      delete (document as { referrer?: string }).referrer
    }
  })

  it('sends no attribution key at all without a campaign or an answer', async () => {
    const { w } = await mountAt('/register')
    await fillAndSubmit(w)
    expect(registerBody()).not.toHaveProperty('attribution')
  })

  it('asks "How did you hear about Roomler?" as an OPTIONAL question: unanswered, sign-up goes through', async () => {
    const { w } = await mountAt('/register')
    expect(w.text()).toContain('How did you hear about Roomler?')
    await fillAndSubmit(w) // the select left empty
    expect(mockApi.post).toHaveBeenCalledWith('/auth/register', expect.any(Object))
    expect(w.text()).not.toMatch(/required/i)
  })

  it('sends the answer as `self_reported`, with or without a campaign', async () => {
    const { w } = await mountAt('/register')
    w.findComponent({ name: 'VSelect' }).vm.$emit('update:modelValue', 'friend')
    await flushPromises()
    await fillAndSubmit(w)
    expect(registerBody().attribution).toEqual({ self_reported: 'friend' })
  })

  it('carries the same attribution on every provider link, and nothing without one', async () => {
    const tagged = await mountAt('/register?utm_source=youtube&utm_campaign=c1&landing_path=%2F')
    tagged.w.findComponent({ name: 'VSelect' }).vm.$emit('update:modelValue', 'youtube')
    await flushPromises()
    for (const p of ['google', 'facebook', 'github', 'linkedin', 'microsoft']) {
      const u = oauthHref(tagged.w, p)
      expect(u.pathname).toBe(`/api/oauth/${p}`)
      expect(Object.fromEntries(u.searchParams)).toEqual({
        utm_source: 'youtube',
        utm_campaign: 'c1',
        landing_path: '/',
        self_reported: 'youtube',
      })
    }
    tagged.w.unmount()

    const plain = await mountAt('/register')
    expect(plain.w.find('a[href^="/api/oauth/google"]').attributes('href')).toBe('/api/oauth/google')
  })

  it('writes nothing to the device: no localStorage, no sessionStorage, no cookie', async () => {
    const { w } = await mountAt('/register?utm_source=youtube&utm_campaign=c1')
    w.findComponent({ name: 'VSelect' }).vm.$emit('update:modelValue', 'reddit')
    await flushPromises()
    oauthHref(w, 'google')
    await fillAndSubmit(w)
    expect(setItem).not.toHaveBeenCalled()
    expect(cookieSet).not.toHaveBeenCalled()
  })
})

describe('RegisterView — the signup goal (FR-88)', () => {
  it('fires once, on the server’s yes', async () => {
    const { w } = await mountAt('/register?utm_source=youtube')
    await fillAndSubmit(w)
    expect(purestat).toHaveBeenCalledTimes(1)
    expect(purestat).toHaveBeenCalledWith('signup')
  })

  it('does not fire when the server refuses', async () => {
    mockApi.post.mockRejectedValueOnce(new Error('Email already registered'))
    const { w } = await mountAt('/register')
    await fillAndSubmit(w)
    expect(purestat).not.toHaveBeenCalled()
  })

  it('is a no-op without purestat: the sign-up still completes and navigates', async () => {
    delete (window as W).purestat
    const { w, router } = await mountAt('/register')
    await fillAndSubmit(w)
    expect(mockApi.post).toHaveBeenCalledWith('/auth/register', expect.any(Object))
    await vi.waitFor(() => expect(router.currentRoute.value.name).toBe('dashboard'))
  })
})
