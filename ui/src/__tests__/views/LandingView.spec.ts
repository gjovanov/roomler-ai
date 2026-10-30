// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-129 B1-s2 — the landing page's install cards follow the server's daemon
// switch for each OS (`GET /api/setup/features`, read through the
// setup-features store): Roomler Setup on the Linux card on
// `wizard_linux_daemon`, on the macOS card on `wizard_macos_daemon`, on both
// for an older server's `wizard_unix_daemon`, and on Windows always. Every
// switch reads false until the server has said true.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createMemoryHistory, createRouter } from 'vue-router'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import LandingView from '@/views/LandingView.vue'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const vuetify = createVuetify({ components, directives })
const blank = { template: '<div />' }

async function mountLanding() {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/', name: 'landing', component: LandingView },
      { path: '/login', name: 'login', component: blank },
      { path: '/register', name: 'register', component: blank },
      { path: '/:rest(.*)*', component: blank },
    ],
  })
  await router.push('/')
  await router.isReady()
  const w = mount(LandingView, {
    global: {
      plugins: [vuetify, router],
      // The app bar needs the app's layout; the rest are not under test.
      stubs: {
        VAppBar: { template: '<div><slot /></div>' },
        ArchitectureGraphic: true,
        VideoFacade: true,
        StayInTouch: true,
        NewsletterPrompt: true,
      },
    },
    attachTo: document.body,
  })
  await flushPromises()
  return w
}

/** The install section's "Roomler Setup" buttons, by the OS they download for. */
function wizardOffered(): string[] {
  const section = document.getElementById('download')!
  return [...section.querySelectorAll('a[href*="/api/setup/"]')]
    .map((a) => a.getAttribute('href')!.replace(/^.*\/api\/setup\//, ''))
    .sort()
}

beforeEach(() => {
  setActivePinia(createPinia())
  mockApi.get.mockReset()
  vi.stubGlobal('fetch', vi.fn(() => Promise.reject(new Error('no plans in this test'))))
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
  document.body.innerHTML = ''
})

describe('LandingView — Roomler Setup on each OS card (FR-129 B1-s2)', () => {
  it('offers it on the Linux card, and not the Mac’s, when the server turns Linux on alone', async () => {
    mockApi.get.mockResolvedValue({
      pairing: true,
      wizard_linux_daemon: true,
      wizard_macos_daemon: false,
      wizard_unix_daemon: false,
      wizard_pairing: false,
    })
    const w = await mountLanding()
    expect(mockApi.get).toHaveBeenCalledWith('/setup/features')
    expect(wizardOffered()).toEqual(['linux', 'windows'])
    w.unmount()
  })

  it('offers it on the Mac’s card alone when the server turns macOS on alone', async () => {
    mockApi.get.mockResolvedValue({
      pairing: true,
      wizard_linux_daemon: false,
      wizard_macos_daemon: true,
      wizard_unix_daemon: false,
      wizard_pairing: false,
    })
    const w = await mountLanding()
    expect(wizardOffered()).toEqual(['macos', 'windows'])
    w.unmount()
  })

  it('offers it on both for an older server’s wizard_unix_daemon', async () => {
    mockApi.get.mockResolvedValue({ pairing: true, wizard_unix_daemon: true, wizard_pairing: false })
    const w = await mountLanding()
    expect(wizardOffered()).toEqual(['linux', 'macos', 'windows'])
    w.unmount()
  })

  it('offers it on Windows only while every switch is off, and when the request fails', async () => {
    mockApi.get.mockResolvedValue({
      pairing: true,
      wizard_linux_daemon: false,
      wizard_macos_daemon: false,
      wizard_unix_daemon: false,
      wizard_pairing: false,
    })
    const off = await mountLanding()
    expect(wizardOffered()).toEqual(['windows'])
    off.unmount()
    document.body.innerHTML = ''

    setActivePinia(createPinia())
    mockApi.get.mockRejectedValue(Object.assign(new Error('API error 404'), { status: 404 }))
    const older = await mountLanding()
    expect(wizardOffered()).toEqual(['windows'])
    older.unmount()
  })
})
