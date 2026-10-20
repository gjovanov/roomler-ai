// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// The Growth page loads its first tab even when the user is not known yet.
//
// The app shell renders before `/auth/me` answers (stores/auth.ts: a hard
// refresh must not wait for it). Growth's loader ran once, immediately, met an
// unknown user, returned — and nothing ran it again until the tab changed, so
// a refresh or a deep link to /observability/growth left the Overview empty.
// Found by FR-116's harness, which loads every page cold.
import { describe, expect, it, vi, beforeEach } from 'vitest'
import { flushPromises, shallowMount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

const hoisted = vi.hoisted(() => ({
  auth: null as unknown as { user: { id: string; is_platform_admin?: boolean } | null },
  growth: {
    digests: [] as unknown[],
    fetchFunnel: vi.fn(),
    fetchCohorts: vi.fn(),
    fetchDigest: vi.fn(),
    fetchOrgs: vi.fn(),
    fetchDigests: vi.fn(),
  },
}))
vi.mock('@/stores/auth', async () => {
  const { reactive } = await import('vue')
  hoisted.auth = reactive({ user: null })
  return { useAuthStore: () => hoisted.auth }
})
vi.mock('@/stores/growth', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/stores/growth')>()),
  useGrowthStore: () => hoisted.growth,
}))

import GrowthView from '@/views/observability/GrowthView.vue'

const vuetify = createVuetify({ components, directives })

beforeEach(() => {
  hoisted.auth.user = null
  hoisted.growth.fetchFunnel.mockReset().mockResolvedValue(null)
  hoisted.growth.fetchCohorts.mockReset().mockResolvedValue({ wao: [], cohorts: [] })
  hoisted.growth.fetchDigest.mockReset().mockResolvedValue({ sections: {} })
})

describe('GrowthView — the first tab loads once the user is known', () => {
  it('a user who resolves AFTER the page rendered still gets the Overview', async () => {
    shallowMount(GrowthView, { global: { plugins: [vuetify] } })
    await flushPromises()
    expect(hoisted.growth.fetchCohorts).not.toHaveBeenCalled()

    hoisted.auth.user = { id: 'u1', is_platform_admin: true }
    await flushPromises()
    expect(hoisted.growth.fetchFunnel).toHaveBeenCalledTimes(1)
    expect(hoisted.growth.fetchCohorts).toHaveBeenCalledTimes(1)
    expect(hoisted.growth.fetchDigest).toHaveBeenCalledTimes(1)
  })

  it('a user known up front loads it once, not twice', async () => {
    hoisted.auth.user = { id: 'u1', is_platform_admin: true }
    shallowMount(GrowthView, { global: { plugins: [vuetify] } })
    await flushPromises()
    expect(hoisted.growth.fetchCohorts).toHaveBeenCalledTimes(1)
  })

  it('a user who is not an operator loads nothing', async () => {
    shallowMount(GrowthView, { global: { plugins: [vuetify] } })
    hoisted.auth.user = { id: 'u2', is_platform_admin: false }
    await flushPromises()
    expect(hoisted.growth.fetchFunnel).not.toHaveBeenCalled()
    expect(hoisted.growth.fetchCohorts).not.toHaveBeenCalled()
  })
})
