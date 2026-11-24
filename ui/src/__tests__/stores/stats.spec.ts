// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

import { api } from '@/api/client'
import { useStatsStore } from '@/stores/stats'

const mockApi = api as unknown as { get: ReturnType<typeof vi.fn> }

describe('stats store', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  it('fetchOverview caches the payload and hits the tenant path', async () => {
    const payload = {
      enabled: true,
      machines: { online: 2, total: 5 },
      calls: { active: 1, minutes_today: 12 },
      spark_machines: [{ t: 1_700_000_000, online: 2 }],
    }
    mockApi.get.mockResolvedValueOnce(payload)
    const store = useStatsStore()
    const out = await store.fetchOverview('t1')
    expect(mockApi.get).toHaveBeenCalledWith('/tenant/t1/stats/overview')
    expect(out).toEqual(payload)
    expect(store.overview?.machines?.online).toBe(2)
  })

  it('fetchOverview swallows errors into null (404 = hidden panel, never a throw into the dashboard)', async () => {
    mockApi.get.mockRejectedValueOnce(new Error('HTTP 404'))
    const store = useStatsStore()
    const out = await store.fetchOverview('t1')
    expect(out).toBeNull()
    expect(store.overview).toBeNull()
    expect(store.error).toContain('404')
  })

  it('range queries interpolate query params per client convention', async () => {
    mockApi.get.mockResolvedValue({ enabled: true, series: [] })
    const store = useStatsStore()
    await store.fetchMachines('t1', '7d')
    expect(mockApi.get).toHaveBeenCalledWith('/tenant/t1/stats/machines?range=7d')
    await store.fetchTunnels('t1', '30d')
    expect(mockApi.get).toHaveBeenCalledWith('/tenant/t1/stats/tunnels?range=30d')
    await store.fetchRelayHistory('us-east', '24h')
    expect(mockApi.get).toHaveBeenCalledWith('/admin/stats/relay/history?region=us-east&range=24h')
    await store.fetchAdminCalls('1y')
    expect(mockApi.get).toHaveBeenCalledWith('/admin/stats/calls?range=1y')
    await store.fetchAdminCalls('1y', 't9')
    expect(mockApi.get).toHaveBeenCalledWith('/admin/stats/calls?tenant_id=t9&range=1y')
  })

  it('fetchRelayCurrent caches for the realtime poll', async () => {
    const payload = {
      enabled: true,
      regions: [{ id: 'us-east', enabled: true, monitored: true, busy: false }],
    }
    mockApi.get.mockResolvedValueOnce(payload)
    const store = useStatsStore()
    await store.fetchRelayCurrent()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/stats/relay/current')
    expect(store.relayCurrent?.regions?.[0]?.id).toBe('us-east')
  })
})

// #4257 — the overview and the mesh are ONE org's. The org dashboard is
// reused when only its org changes, and its poll and its org watch can each
// have a fetch in flight: an answer for the org asked before must not land
// over the org asked after, and a fetch for another org must not show the
// previous org's numbers while it loads.
describe('stats store: one org at a time (#4257)', () => {
  const ORG_A = 'a'.repeat(24)
  const ORG_B = 'b'.repeat(24)
  const OVERVIEW_A = { enabled: true, machines: { online: 7, total: 9 } }
  const OVERVIEW_B = { enabled: true, machines: { online: 2, total: 3 } }
  const MESH_A = { enabled: true, nodes: [{ id: 'node-a' }] }
  const MESH_B = { enabled: true, nodes: [{ id: 'node-b' }] }

  function deferred() {
    let resolve!: (v: unknown) => void
    let reject!: (e: unknown) => void
    const promise = new Promise((res, rej) => {
      resolve = res
      reject = rej
    })
    return { promise, resolve, reject }
  }

  beforeEach(() => {
    setActivePinia(createPinia())
    // clearAllMocks keeps a queued answer: start from none.
    mockApi.get.mockReset()
  })

  it("a fetch for another org drops the previous org's overview and mesh at once", async () => {
    const store = useStatsStore()
    mockApi.get.mockResolvedValueOnce(OVERVIEW_A)
    mockApi.get.mockResolvedValueOnce(MESH_A)
    await store.fetchOverview(ORG_A)
    await store.fetchMesh(ORG_A)
    const overviewB = deferred()
    const meshB = deferred()
    mockApi.get.mockReturnValueOnce(overviewB.promise)
    mockApi.get.mockReturnValueOnce(meshB.promise)

    const o = store.fetchOverview(ORG_B)
    const m = store.fetchMesh(ORG_B)
    expect(store.overview).toBeNull()
    expect(store.mesh).toBeNull()

    overviewB.resolve(OVERVIEW_B)
    meshB.resolve(MESH_B)
    await o
    await m
    expect(store.overview).toEqual(OVERVIEW_B)
    expect(store.mesh).toEqual(MESH_B)
  })

  it('an overview and a mesh answered for org A after org B was asked are dropped', async () => {
    const store = useStatsStore()
    const overviewA = deferred()
    const meshA = deferred()
    mockApi.get.mockReturnValueOnce(overviewA.promise)
    mockApi.get.mockReturnValueOnce(meshA.promise)
    const oa = store.fetchOverview(ORG_A)
    const ma = store.fetchMesh(ORG_A)
    mockApi.get.mockResolvedValueOnce(OVERVIEW_B)
    mockApi.get.mockResolvedValueOnce(MESH_B)
    await store.fetchOverview(ORG_B)
    await store.fetchMesh(ORG_B)

    overviewA.resolve(OVERVIEW_A)
    meshA.resolve(MESH_A)
    await oa
    await ma
    expect(store.overview).toEqual(OVERVIEW_B)
    expect(store.mesh).toEqual(MESH_B)
  })

  it("a failure for org A that lands after org B was asked leaves org B's overview and mesh", async () => {
    const store = useStatsStore()
    const overviewA = deferred()
    const meshA = deferred()
    mockApi.get.mockReturnValueOnce(overviewA.promise)
    mockApi.get.mockReturnValueOnce(meshA.promise)
    const oa = store.fetchOverview(ORG_A)
    const ma = store.fetchMesh(ORG_A)
    mockApi.get.mockResolvedValueOnce(OVERVIEW_B)
    mockApi.get.mockResolvedValueOnce(MESH_B)
    await store.fetchOverview(ORG_B)
    await store.fetchMesh(ORG_B)

    overviewA.reject(new Error('HTTP 500'))
    meshA.reject(new Error('HTTP 500'))
    await oa
    await ma
    expect(store.overview).toEqual(OVERVIEW_B)
    expect(store.mesh).toEqual(MESH_B)
    expect(store.error).toBeNull()
  })
})
