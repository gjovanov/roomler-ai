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

import { useTunnelClientStore, type TunnelClient } from '@/stores/tunnelClients'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const TENANT_ID = 'ten_1'

function mkClient(over: Partial<TunnelClient> = {}): TunnelClient {
  return {
    id: 'c1',
    tenant_id: TENANT_ID,
    owner_user_id: 'u1',
    name: 'laptop',
    machine_id: 'mach-1',
    os: 'linux',
    client_version: '0.3.0',
    status: 'online',
    last_seen_at: '2026-07-28T00:00:00Z',
    ...over,
  }
}

describe('useTunnelClientStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  it('fetchTunnelClients populates the list and total', async () => {
    mockApi.get.mockResolvedValueOnce({
      items: [mkClient(), mkClient({ id: 'c2' })],
      total: 2,
      page: 1,
      per_page: 20,
      total_pages: 1,
    })
    const store = useTunnelClientStore()
    await store.fetchTunnelClients(TENANT_ID)
    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/tunnel-client?per_page=100`)
    expect(store.clients).toHaveLength(2)
    expect(store.total).toBe(2)
  })

  it('fetchTunnelClients clears and records the error on failure', async () => {
    mockApi.get.mockRejectedValueOnce(new Error('boom'))
    const store = useTunnelClientStore()
    await store.fetchTunnelClients(TENANT_ID)
    expect(store.clients).toEqual([])
    expect(store.total).toBe(0)
    expect(store.error).toBe('boom')
  })

  // #4553 — nothing writes `online` to a tunnel client's stored status, so a
  // count of that field read 0 with every client connected.
  it('counts the clients online by their presence, never by their stored status', async () => {
    mockApi.get.mockResolvedValueOnce({
      items: [
        mkClient({ id: 'c1', status: 'offline', presence: 'online', is_online: true }),
        mkClient({ id: 'c2', status: 'offline', presence: 'online', is_online: true }),
        mkClient({ id: 'c3', status: 'offline', presence: 'stale', is_online: false }),
        mkClient({ id: 'c4', status: 'online', presence: 'offline', is_online: false }),
      ],
      total: 4,
      page: 1,
      per_page: 100,
      total_pages: 1,
    })
    const store = useTunnelClientStore()
    await store.fetchTunnelClients(TENANT_ID)
    expect(store.onlineCount).toBe(2)
  })

  it('issueEnrollmentToken POSTs the enroll-token path', async () => {
    mockApi.post.mockResolvedValueOnce({
      enrollment_token: 'tok',
      expires_in: 600,
      jti: 'j1',
    })
    const store = useTunnelClientStore()
    const tok = await store.issueEnrollmentToken(TENANT_ID)
    expect(mockApi.post).toHaveBeenCalledWith(
      `/tenant/${TENANT_ID}/tunnel-client/enroll-token`,
    )
    expect(tok.enrollment_token).toBe('tok')
  })

  it('deleteTunnelClient removes the row and decrements the total', async () => {
    mockApi.delete.mockResolvedValueOnce({
      deleted: true,
      overlay_released: true,
      overlay_ip: '100.64.0.5',
    })
    const store = useTunnelClientStore()
    store.clients = [mkClient({ id: 'c1' }), mkClient({ id: 'c2' })]
    store.total = 2

    const res = await store.deleteTunnelClient(TENANT_ID, 'c1')

    expect(mockApi.delete).toHaveBeenCalledWith(
      `/tenant/${TENANT_ID}/tunnel-client/c1`,
    )
    expect(store.clients.map((c) => c.id)).toEqual(['c2'])
    expect(store.total).toBe(1)
    expect(res.overlay_ip).toBe('100.64.0.5')
  })

  it('the total never goes below zero', async () => {
    mockApi.delete.mockResolvedValueOnce({
      deleted: true,
      overlay_released: false,
      overlay_ip: null,
    })
    const store = useTunnelClientStore()
    store.clients = [mkClient({ id: 'c1' })]
    store.total = 0 // out of sync with the list — don't go negative

    await store.deleteTunnelClient(TENANT_ID, 'c1')
    expect(store.total).toBe(0)
  })

  // Locks the await-before-mutate ordering: a rejected delete must leave the
  // admin's view untouched.
  it('a rejected delete leaves clients and total intact', async () => {
    mockApi.delete.mockRejectedValueOnce(new Error('forbidden'))
    const store = useTunnelClientStore()
    store.clients = [mkClient({ id: 'c1' }), mkClient({ id: 'c2' })]
    store.total = 2

    await expect(store.deleteTunnelClient(TENANT_ID, 'c1')).rejects.toThrow(
      'forbidden',
    )
    expect(store.clients.map((c) => c.id)).toEqual(['c1', 'c2'])
    expect(store.total).toBe(2)
  })
})

// #4248 — the store holds one org's tunnel clients at a time, and knows
// which. A page that only names or picks them asks with
// `ensureTunnelClients`, which fetches only when the store does not hold that
// org's clients.
describe('useTunnelClientStore: one org at a time (#4248)', () => {
  const ORG_A = 'org_a'
  const ORG_B = 'org_b'
  const listOf = (ids: string[]) => ({
    items: ids.map((id) => mkClient({ id })),
    total: ids.length,
    page: 1,
    per_page: 100,
    total_pages: 1,
  })
  const asked = () => mockApi.get.mock.calls.map((c) => c[0])

  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  it("asked for org B while holding org A's: one fetch, for org B", async () => {
    const store = useTunnelClientStore()
    mockApi.get.mockResolvedValueOnce(listOf(['a1']))
    await store.fetchTunnelClients(ORG_A)
    mockApi.get.mockResolvedValueOnce(listOf(['b1']))
    await store.ensureTunnelClients(ORG_B)
    expect(asked()).toEqual([
      `/tenant/${ORG_A}/tunnel-client?per_page=100`,
      `/tenant/${ORG_B}/tunnel-client?per_page=100`,
    ])
    expect(store.clients.map((c) => c.id)).toEqual(['b1'])
    expect(store.clientsTenantId).toBe(ORG_B)
  })

  it("asked for org A while holding org A's, or loading them: no fetch", async () => {
    const store = useTunnelClientStore()
    let answer!: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => (answer = r)))
    const first = store.fetchTunnelClients(ORG_A)
    await store.ensureTunnelClients(ORG_A)
    answer(listOf(['a1']))
    await first
    await store.ensureTunnelClients(ORG_A)
    expect(asked()).toEqual([`/tenant/${ORG_A}/tunnel-client?per_page=100`])
    expect(store.clients.map((c) => c.id)).toEqual(['a1'])
  })

  it('asked again after a failed fetch: fetches', async () => {
    const store = useTunnelClientStore()
    mockApi.get.mockRejectedValueOnce(new Error('boom'))
    await store.fetchTunnelClients(ORG_A)
    mockApi.get.mockResolvedValueOnce(listOf(['a1']))
    await store.ensureTunnelClients(ORG_A)
    expect(mockApi.get).toHaveBeenCalledTimes(2)
    expect(store.clients.map((c) => c.id)).toEqual(['a1'])
    expect(store.error).toBeNull()
  })

  it("a fetch for org B drops org A's clients at once, and org A's late answer", async () => {
    const store = useTunnelClientStore()
    mockApi.get.mockResolvedValueOnce(listOf(['a1']))
    await store.fetchTunnelClients(ORG_A)
    let lateA!: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => (lateA = r)))
    const refreshA = store.fetchTunnelClients(ORG_A)
    // A refresh of the same org keeps what it shows while it loads.
    expect(store.clients.map((c) => c.id)).toEqual(['a1'])

    let answerB!: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => (answerB = r)))
    const loadB = store.fetchTunnelClients(ORG_B)
    expect(store.clients).toEqual([])
    expect(store.total).toBe(0)

    answerB(listOf(['b1']))
    await loadB
    lateA(listOf(['a1', 'a2']))
    await refreshA
    expect(store.clients.map((c) => c.id)).toEqual(['b1'])
    expect(store.total).toBe(1)
    expect(store.loading).toBe(false)
  })
})
