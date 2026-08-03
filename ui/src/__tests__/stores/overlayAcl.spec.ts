import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import { useOverlayAclStore, type OverlayPolicy } from '@/stores/overlayAcl'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const TENANT_ID = '69a1dbbad2000f26adc875ce'

function mkPolicy(over: Partial<OverlayPolicy> = {}): OverlayPolicy {
  return {
    id: '6a6f4a76719fbb3fcc7f8102',
    tenant_id: TENANT_ID,
    name: 'devs-reach-k8s',
    enabled: true,
    sources: [{ kind: 'all_nodes' }],
    via: [{ kind: 'all_nodes' }],
    destinations: [
      { cidr: '10.84.6.0/24', port_range: { low: 1, high: 65535 }, proto: 'any' },
    ],
    created_at: '2026-08-03T00:00:00Z',
    updated_at: '2026-08-03T00:00:00Z',
    ...over,
  }
}

beforeEach(() => {
  setActivePinia(createPinia())
  vi.clearAllMocks()
})

describe('overlayAcl store', () => {
  it('fetches policies and the tenant posture in one round-trip', async () => {
    mockApi.get.mockResolvedValue({
      items: [mkPolicy()],
      total: 1,
      page: 1,
      per_page: 25,
      mode: 'warn',
    })
    const store = useOverlayAclStore()
    await store.fetchPolicies(TENANT_ID)

    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/overlay-acl`)
    expect(store.policies).toHaveLength(1)
    expect(store.total).toBe(1)
    // The posture rides the list response so the UI can render the banner
    // without a second call.
    expect(store.mode).toBe('warn')
    expect(store.error).toBeNull()
  })

  it('defaults the posture to off, so the feature is inert until opted in', () => {
    expect(useOverlayAclStore().mode).toBe('off')
  })

  it('sets error and clears the collection when the fetch fails', async () => {
    mockApi.get.mockRejectedValue(new Error('API error 500'))
    const store = useOverlayAclStore()
    store.policies = [mkPolicy()]
    await store.fetchPolicies(TENANT_ID)

    expect(store.error).toBe('API error 500')
    expect(store.policies).toEqual([])
    expect(store.total).toBe(0)
  })

  it('prepends a created policy', async () => {
    const existing = mkPolicy({ id: 'aaa', name: 'old' })
    const created = mkPolicy({ id: 'bbb', name: 'new' })
    mockApi.post.mockResolvedValue(created)
    const store = useOverlayAclStore()
    store.policies = [existing]
    store.total = 1

    await store.createPolicy(TENANT_ID, {
      name: 'new',
      enabled: true,
      sources: [{ kind: 'all_nodes' }],
      via: [{ kind: 'all_nodes' }],
      destinations: created.destinations,
    })

    expect(store.policies.map((p) => p.id)).toEqual(['bbb', 'aaa'])
    expect(store.total).toBe(2)
  })

  it('replaces the row in place on update', async () => {
    const updated = mkPolicy({ name: 'renamed' })
    mockApi.put.mockResolvedValue(updated)
    const store = useOverlayAclStore()
    store.policies = [mkPolicy()]

    await store.updatePolicy(TENANT_ID, updated.id, {
      name: 'renamed',
      enabled: true,
      sources: updated.sources,
      via: updated.via,
      destinations: updated.destinations,
    })

    expect(store.policies[0].name).toBe('renamed')
    expect(store.policies).toHaveLength(1)
  })

  it('removes the row and decrements total on delete', async () => {
    mockApi.delete.mockResolvedValue({ deleted: true })
    const store = useOverlayAclStore()
    const p = mkPolicy()
    store.policies = [p]
    store.total = 1

    await store.deletePolicy(TENANT_ID, p.id)

    expect(mockApi.delete).toHaveBeenCalledWith(
      `/tenant/${TENANT_ID}/overlay-acl/${p.id}`,
    )
    expect(store.policies).toEqual([])
    expect(store.total).toBe(0)
  })

  it('mutations throw so the component can surface the message', async () => {
    mockApi.post.mockRejectedValue(new Error('API error 400'))
    const store = useOverlayAclStore()
    await expect(
      store.createPolicy(TENANT_ID, {
        name: 'bad',
        enabled: true,
        sources: [{ kind: 'all_nodes' }],
        via: [{ kind: 'all_nodes' }],
        destinations: [],
      }),
    ).rejects.toThrow('API error 400')
    // Store state must be untouched by a failed mutation.
    expect(store.policies).toEqual([])
    expect(store.error).toBeNull()
  })

  it('setMode round-trips the posture', async () => {
    mockApi.put.mockResolvedValue({ mode: 'enforce' })
    const store = useOverlayAclStore()
    await store.setMode(TENANT_ID, 'enforce')

    expect(mockApi.put).toHaveBeenCalledWith(
      `/tenant/${TENANT_ID}/overlay-acl/mode`,
      { mode: 'enforce' },
    )
    expect(store.mode).toBe('enforce')
  })
})
