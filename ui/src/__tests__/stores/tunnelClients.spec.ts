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
    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/tunnel-client`)
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
