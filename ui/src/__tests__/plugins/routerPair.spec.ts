// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-129 (#3022) A2 — the app router's part: the `/pair` record (no auth, no
// guest, gated on `fleet`, no code in the URL), and the signed-in guest
// branch of the guard, which brings a signed-in tab back to `/pair` while a
// pairing waits, after the invite and the same-tab redirect it already
// honoured.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(async (path: string) =>
      path === '/capabilities'
        ? { version: 'test', modules: ['chat', 'conference', 'fleet', 'remote', 'network'] }
        : undefined,
    ),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
  ApiError: class extends Error {},
}))
// The destinations' views are not what is under test; keep the module graph small.
const Stub = { template: '<div />' }
vi.mock('@/views/pair/PairView.vue', () => ({ default: Stub }))
vi.mock('@/views/auth/LoginView.vue', () => ({ default: Stub }))
vi.mock('@/views/auth/RegisterView.vue', () => ({ default: Stub }))
vi.mock('@/views/invite/InviteLandingView.vue', () => ({ default: Stub }))
vi.mock('@/views/NotFoundView.vue', () => ({ default: Stub }))
vi.mock('@/components/layout/AppLayout.vue', () => ({ default: { template: '<router-view />' } }))
vi.mock('@/views/dashboard/DashboardView.vue', () => ({ default: Stub }))

import router from '@/plugins/router'
import { PENDING_PAIR_KEY, PENDING_PAIR_TTL_MS, rememberPendingPair } from '@/utils/pairing'

beforeEach(() => {
  setActivePinia(createPinia())
  localStorage.clear()
  sessionStorage.clear()
})

afterEach(() => {
  localStorage.clear()
  sessionStorage.clear()
})

describe('the /pair route', () => {
  it('exists without auth or guest meta, gated on fleet, and takes no code in the URL', () => {
    const record = router.getRoutes().find((r) => r.name === 'pair')
    expect(record, 'no /pair route').toBeDefined()
    expect(record!.path).toBe('/pair')
    expect(record!.meta.auth).toBeUndefined()
    expect(record!.meta.guest).toBeUndefined()
    expect(record!.meta.module).toBe('fleet')
    // Decision 1: the link never carries the code. A path with one is not this page.
    expect(router.resolve('/pair').name).toBe('pair')
    expect(router.resolve('/pair/BCDFGHJK').name).toBe('not-found')
    expect(router.resolve('/pair').params).toEqual({})
  })

  it('lets a signed-out visitor in (the page offers sign-in itself)', async () => {
    localStorage.removeItem('roomler-signed-in')
    await router.push('/pair')
    expect(router.currentRoute.value.path).toBe('/pair')
  })
})

describe('the guard’s signed-in guest branch', () => {
  beforeEach(() => {
    localStorage.setItem('roomler-signed-in', '1')
  })

  it('sends a signed-in tab that lands on a guest page to /pair while a pairing waits, and spends the marker', async () => {
    rememberPendingPair()
    await router.push('/login')
    expect(router.currentRoute.value.path).toBe('/pair')
    expect(localStorage.getItem(PENDING_PAIR_KEY)).toBeNull()
  })

  it('an expired marker sends nobody anywhere but the dashboard, and is cleared', async () => {
    rememberPendingPair(Date.now() - PENDING_PAIR_TTL_MS - 1)
    await router.push('/login')
    expect(router.currentRoute.value.name).toBe('dashboard')
    expect(localStorage.getItem(PENDING_PAIR_KEY)).toBeNull()
  })

  it('the invite and the same-tab redirect keep their precedence', async () => {
    rememberPendingPair()
    sessionStorage.setItem('pending_invite_code', 'inv-1')
    await router.push('/register')
    expect(router.currentRoute.value.name).toBe('invite')
    expect(router.currentRoute.value.params.code).toBe('inv-1')
    // Not consumed by the invite: the pairing still waits.
    expect(localStorage.getItem(PENDING_PAIR_KEY)).not.toBeNull()

    sessionStorage.setItem('pending_redirect', '/pair')
    await router.push('/login')
    expect(router.currentRoute.value.path).toBe('/pair')
    expect(sessionStorage.getItem('pending_redirect')).toBeNull()
    expect(localStorage.getItem(PENDING_PAIR_KEY)).not.toBeNull()
  })

  it('the destination is the literal /pair, whatever the marker holds', async () => {
    // The marker is a deadline, not a URL: a tampered value cannot steer the redirect.
    localStorage.setItem(PENDING_PAIR_KEY, String(Date.now() + 60_000))
    await router.push('/login')
    expect(router.currentRoute.value.path).toBe('/pair')
    localStorage.setItem(PENDING_PAIR_KEY, '//evil.example')
    await router.push('/login')
    expect(router.currentRoute.value.name).toBe('dashboard')
  })
})
