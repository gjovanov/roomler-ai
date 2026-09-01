// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-58 follow-up — the confirm PAGE. The load-bearing assertions: merely
// LOADING the page performs no request at all (a prefetcher that renders the
// page must still confirm nothing — the field finding that created this
// view), and only the button's POST flips state, with an explicit
// `confirmed: true` as the only success.
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createMemoryHistory, createRouter } from 'vue-router'
import NewsletterConfirmView from '@/views/newsletter/NewsletterConfirmView.vue'

const vuetify = createVuetify({ components, directives })

async function mountView() {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/', component: { template: '<div />' } },
      { path: '/newsletter/confirm/:token', component: { template: '<div />' } },
    ],
  })
  await router.push('/newsletter/confirm/tok-abc123')
  return mount(NewsletterConfirmView, { global: { plugins: [vuetify, router] } })
}

describe('NewsletterConfirmView', () => {
  beforeEach(() => {
    vi.stubGlobal('fetch', vi.fn())
    // jsdom has no ResizeObserver; the button's :loading spinner
    // (VProgressCircular) requires one.
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
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })

  it('loading the page performs NO request — prefetch-proof by construction', async () => {
    const w = await mountView()
    expect(w.text()).toContain('One click to confirm')
    expect(fetch).not.toHaveBeenCalled()
  })

  it('the button POSTs the token and an explicit true is the only success', async () => {
    vi.mocked(fetch).mockResolvedValue(
      new Response(JSON.stringify({ confirmed: true }), { status: 200 }),
    )
    const w = await mountView()
    await w.get('button').trigger('click')
    await vi.waitFor(() => expect(w.text()).toContain("You're on the list"))
    expect(fetch).toHaveBeenCalledWith('/api/subscribe/confirm/tok-abc123', { method: 'POST' })
  })

  it('a burned or unknown token is told so, never told success', async () => {
    vi.mocked(fetch).mockResolvedValue(
      new Response(JSON.stringify({ confirmed: false }), { status: 200 }),
    )
    const w = await mountView()
    await w.get('button').trigger('click')
    await vi.waitFor(() => expect(w.text()).toContain("That link didn't work"))
    expect(w.text()).not.toContain("You're on the list")
  })

  it('a transport failure says nothing was changed and offers retry', async () => {
    vi.mocked(fetch).mockRejectedValue(new TypeError('offline'))
    const w = await mountView()
    await w.get('button').trigger('click')
    await vi.waitFor(() => expect(w.text()).toContain('Could not reach the server'))
    expect(w.text()).toContain('Try again')
  })
})
