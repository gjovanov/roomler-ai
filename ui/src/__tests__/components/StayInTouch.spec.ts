// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-88 (#1790) — the SPA's newsletter form: the campaign on the page URL
// stands in for the form's `source` (§3a), and `subscribe` fires after the
// 202 (§3b) — a no-op without purestat, and never on a failure.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import StayInTouch from '@/components/landing/StayInTouch.vue'
import { api } from '@/api/client'

const vuetify = createVuetify({ components, directives })
const mockApi = vi.mocked(api)
type W = Window & { purestat?: unknown }
const purestat = vi.fn()

async function subscribe(url: string, source = 'landing') {
  window.history.replaceState({}, '', url)
  const w = mount(StayInTouch, {
    props: { source },
    global: { plugins: [vuetify], stubs: { RouterLink: true } },
  })
  await w.find('input[type="email"]').setValue('someone@example.com')
  await w.find('form').trigger('submit')
  await flushPromises()
  return w
}

beforeEach(() => {
  // jsdom has none; the button's :loading spinner (VProgressCircular) needs one.
  vi.stubGlobal(
    'ResizeObserver',
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  )
  mockApi.post.mockReset()
  mockApi.post.mockResolvedValue(undefined)
  purestat.mockReset()
  ;(window as W).purestat = purestat
})

afterEach(() => {
  delete (window as W).purestat
  vi.unstubAllGlobals()
  window.history.replaceState({}, '', '/')
})

describe('StayInTouch (FR-88)', () => {
  it('posts the page’s campaign as the source', async () => {
    await subscribe('/landing?utm_source=youtube&utm_campaign=fr88-test')
    expect(mockApi.post).toHaveBeenCalledWith('/subscribe', { email: 'someone@example.com', source: 'fr88-test' })
  })

  it('keeps its own source without a campaign', async () => {
    await subscribe('/landing?utm_source=youtube', 'landing-footer')
    expect(mockApi.post).toHaveBeenCalledWith('/subscribe', { email: 'someone@example.com', source: 'landing-footer' })
  })

  it('fires the subscribe goal once, after the 202', async () => {
    await subscribe('/landing')
    expect(purestat).toHaveBeenCalledTimes(1)
    expect(purestat).toHaveBeenCalledWith('subscribe')
  })

  it('fires nothing when the request fails', async () => {
    mockApi.post.mockRejectedValueOnce(new Error('offline'))
    await subscribe('/landing')
    expect(purestat).not.toHaveBeenCalled()
  })

  it('still confirms without purestat', async () => {
    delete (window as W).purestat
    const w = await subscribe('/landing')
    expect(w.text()).toContain('check your inbox')
  })
})
