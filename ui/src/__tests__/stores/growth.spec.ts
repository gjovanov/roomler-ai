// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-102 P1d — the growth store: the three cells stay three (a measured
// value, `not_measured`, a measured zero), the day keeps its 7/28-day
// context, and the four calls hit the routes the server mounts.
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

import {
  useGrowthStore,
  isNotMeasured,
  isFigure,
  notMeasuredLabel,
  figureLabel,
  humanKey,
  rateIfEnough,
  SMALL_N,
} from '@/stores/growth'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

describe('growth helpers', () => {
  it('tells not-measured from a measured zero', () => {
    expect(isNotMeasured({ not_measured: 'purestat key not configured' })).toBe(true)
    expect(isNotMeasured({ day: 0, d7: 0, d28: 0 })).toBe(false)
    expect(isNotMeasured(0)).toBe(false)
    expect(isNotMeasured(null)).toBe(false)
    expect(isFigure({ day: 0, d7: 0, d28: 0 })).toBe(true)
    expect(isFigure({ day: 1 })).toBe(false)
    expect(isFigure({ not_measured: 'x' })).toBe(false)
  })

  it('labels a not-measured cell with its phase, never a number', () => {
    expect(notMeasuredLabel({ not_measured: 'install', arrives_with: 'P3' })).toBe(
      'not measured — arrives with P3',
    )
    expect(notMeasuredLabel({ not_measured: 'purestat key not configured' })).toBe(
      'not measured — purestat key not configured',
    )
    expect(figureLabel({ day: 2, d7: 9, d28: 31 })).toBe('2 (7d 9 · 28d 31)')
    expect(humanKey('orgs_devices')).toBe('Orgs devices')
  })

  it('shows a rate only once the settled denominator reaches the n-floor', () => {
    // P2 — below SMALL_N the UI shows counts and hides the percentage.
    expect(rateIfEnough(5, SMALL_N - 1)).toBeNull()
    expect(rateIfEnough(0, 0)).toBeNull()
    expect(rateIfEnough(15, 30)).toBe(50)
    expect(rateIfEnough(1, 300)).toBe(0.3)
    expect(SMALL_N).toBe(30)
  })
})

describe('useGrowthStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  it('lists digests from the admin route and keeps them', async () => {
    mockApi.get.mockResolvedValueOnce({
      enabled: true,
      digests: [{ day: '2026-10-13', status: 'stored', attempts: 1, recipients: 0, excluded: { users: 1, tenants: 2 } }],
    })
    const store = useGrowthStore()
    const rows = await store.fetchDigests(undefined, 10)
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/digests?limit=10')
    expect(rows).toHaveLength(1)
    expect(store.digests[0].day).toBe('2026-10-13')
    expect(store.error).toBeNull()
  })

  it('a missing day is null, not an error; other failures throw', async () => {
    mockApi.get.mockRejectedValueOnce(Object.assign(new Error('nf'), { status: 404 }))
    const store = useGrowthStore()
    expect(await store.fetchDigest('2026-10-13')).toBeNull()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/digest?day=2026-10-13')
    mockApi.get.mockRejectedValueOnce(Object.assign(new Error('boom'), { status: 500 }))
    await expect(store.fetchDigest()).rejects.toThrow('boom')
    expect(mockApi.get).toHaveBeenLastCalledWith('/admin/growth/digest')
  })

  it('recompute never asks to send unless told, and the preview carries include_internal', async () => {
    mockApi.post.mockResolvedValueOnce({ day: '2026-10-13', status: 'stored' })
    mockApi.get.mockResolvedValueOnce({ day: '2026-10-13', include_internal: true, sections: {} })
    const store = useGrowthStore()
    await store.runDigest('2026-10-13')
    expect(mockApi.post).toHaveBeenCalledWith('/admin/growth/digest/run?day=2026-10-13&send=false')
    await store.fetchPreview('2026-10-13', true)
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/digest/preview?day=2026-10-13&include_internal=true',
    )
  })

  it('fetches the funnel, cohorts, orgs and a timeline at the P2 routes', async () => {
    const store = useGrowthStore()
    mockApi.get.mockResolvedValue({})
    await store.fetchFunnel()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/funnel')
    await store.fetchFunnel({ by: 'source', includeInternal: true })
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/funnel?by=source&include_internal=true',
    )
    await store.fetchCohorts()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/cohorts?unit=week')
    await store.fetchOrgs(50)
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/orgs?limit=50')
    await store.fetchTimeline('69a1dbbad2000f26adc875ce')
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/org/69a1dbbad2000f26adc875ce/timeline',
    )
  })
})
