// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-88 (#1790) §3b — the OAuth half of the `signup` goal. The callback fires
// it only when the server marked the callback as the one that CREATED the
// account (`signup=1`), only once the session is real, and only after the
// URL has been cleaned: purestat sends `location.href` with every event, and
// this page's URL arrives with the access token in its fragment.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createMemoryHistory, createRouter } from 'vue-router'

vi.mock('@/stores/auth', () => ({ useAuthStore: () => ({ fetchMe: vi.fn(() => Promise.resolve()) }) }))
vi.mock('@/stores/ws', () => ({ useWsStore: () => ({ connect: vi.fn() }) }))
vi.mock('@/api/session', () => ({ markSignedIn: vi.fn(), clearSignedIn: vi.fn() }))

import OAuthCallbackView from '@/views/auth/OAuthCallbackView.vue'

const vuetify = createVuetify({ components, directives })
type W = Window & { purestat?: unknown }

/** Every goal, with the URL purestat would have sent alongside it. */
let events: Array<{ goal: string; href: string }>

async function land(url: string, sessionOk = true) {
  window.history.replaceState({}, '', url)
  vi.stubGlobal('fetch', vi.fn(() => Promise.resolve({ ok: sessionOk })))
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/oauth/callback', component: OAuthCallbackView },
      { path: '/', name: 'dashboard', component: { template: '<div />' } },
      { path: '/login', component: { template: '<div />' } },
    ],
  })
  await router.push('/oauth/callback')
  await router.isReady()
  const w = mount(OAuthCallbackView, { global: { plugins: [vuetify, router] } })
  await flushPromises()
  return w
}

beforeEach(() => {
  events = []
  ;(window as W).purestat = (goal: string) => events.push({ goal, href: window.location.href })
  // jsdom has none; the page's spinner (VProgressCircular) needs one.
  vi.stubGlobal(
    'ResizeObserver',
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  )
})

afterEach(() => {
  delete (window as W).purestat
  vi.unstubAllGlobals()
  window.history.replaceState({}, '', '/')
})

describe('OAuthCallbackView — the signup goal (FR-88)', () => {
  it('fires once when the server marked a NEW account, after the token left the URL', async () => {
    await land('/oauth/callback#token=secret-jwt&signup=1')
    expect(events.map((e) => e.goal)).toEqual(['signup'])
    expect(events[0]!.href).not.toContain('secret-jwt')
    expect(events[0]!.href).not.toContain('#')
    expect(window.location.hash).toBe('')
  })

  it('accepts the marker in the query too', async () => {
    await land('/oauth/callback?signup=1#token=secret-jwt')
    expect(events.map((e) => e.goal)).toEqual(['signup'])
    expect(events[0]!.href).not.toContain('secret-jwt')
  })

  it('does not fire for a sign-in to an existing account (no marker)', async () => {
    await land('/oauth/callback#token=secret-jwt')
    expect(events).toEqual([])
  })

  it('does not fire when the session is not real, marker or not', async () => {
    const w = await land('/oauth/callback#token=secret-jwt&signup=1', false)
    expect(events).toEqual([])
    expect(w.text()).toContain('Failed to complete OAuth login')
  })

  it('is a no-op without purestat', async () => {
    delete (window as W).purestat
    const w = await land('/oauth/callback#token=secret-jwt&signup=1')
    expect(w.text()).not.toContain('Failed to complete OAuth login')
  })
})
