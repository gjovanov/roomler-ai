// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import { useTunnelPolicyStore, type TunnelPolicy } from '@/stores/tunnelPolicies'
import { useTenantStore } from '@/stores/tenant'
import { api } from '@/api/client'
import { pagedRoute, rowId } from './pagedRoute'

const mockApi = vi.mocked(api)
const TENANT_ID = '69a1dbbad2000f26adc875ce'
const VIEW_CHANNELS = 1 << 0
const MANAGE_AGENTS = 1 << 24
const REMOTE_CONTROL = 1 << 25

/** The caller's mask for TENANT_ID, already known: `ensureMyMembership`
 *  returns at once, as it does once AppLayout's `/member/me` has landed.
 *  `null` with the tenant set is a membership fetch that failed. */
function asCaller(mask: number | null, isOwner = false) {
  const t = useTenantStore()
  t.isOwner = isOwner
  t.myPermissions = mask
  t.myPermissionsTenantId = TENANT_ID
}

function mkPolicy(over: Partial<TunnelPolicy> = {}): TunnelPolicy {
  return {
    id: '6a6f4a76719fbb3fcc7f8102',
    tenant_id: TENANT_ID,
    name: 'ops-prod-postgres',
    subjects: [{ kind: 'all_users' }],
    targets: [{ kind: 'all_agents' }],
    allowlist: [
      {
        host_pattern: { kind: 'exact', value: 'db.intranet' },
        port_range: { low: 5432, high: 5432 },
        proto: 'tcp',
      },
    ],
    max_concurrent_flows: null,
    max_bytes_per_session: null,
    created_at: '2026-11-10T00:00:00Z',
    updated_at: '2026-11-10T00:00:00Z',
    ...over,
  }
}

function listOf(items: TunnelPolicy[]) {
  return { items, total: items.length, page: 1, per_page: 25, total_pages: 1 }
}

beforeEach(() => {
  setActivePinia(createPinia())
  vi.clearAllMocks()
})

// Every tunnel-policy route is MANAGE_AGENTS on the server, the reads too.
describe('tunnelPolicies store: who may manage the org tunnel ACL', () => {
  it('a holder of MANAGE_AGENTS lists the policies', async () => {
    asCaller(MANAGE_AGENTS)
    mockApi.get.mockResolvedValue(listOf([mkPolicy()]))
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(TENANT_ID)

    expect(mockApi.get).toHaveBeenCalledWith(
      `/tenant/${TENANT_ID}/tunnel-policy?page=1&per_page=100`,
    )
    expect(store.policies).toHaveLength(1)
    expect(store.total).toBe(1)
    expect(store.refused).toBe(false)
    expect(store.error).toBeNull()
  })

  it('the owner lists them whatever the mask says', async () => {
    asCaller(0, true)
    mockApi.get.mockResolvedValue(listOf([]))
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(TENANT_ID)

    expect(mockApi.get).toHaveBeenCalledTimes(1)
    expect(store.refused).toBe(false)
  })

  it('a member without MANAGE_AGENTS sends nothing and is refused, not errored', async () => {
    // REMOTE_CONTROL draws the Network pages, but it is not the bit.
    asCaller(VIEW_CHANNELS + REMOTE_CONTROL)
    const store = useTunnelPolicyStore()
    store.policies = [mkPolicy()]
    store.total = 1
    await store.fetchPolicies(TENANT_ID)

    expect(mockApi.get).not.toHaveBeenCalled()
    expect(store.refused).toBe(true)
    expect(store.error).toBeNull()
    expect(store.policies).toEqual([])
    expect(store.total).toBe(0)
    expect(store.loading).toBe(false)
  })

  it('an unknown mask fails open: the server decides, and its 403 is the same refusal', async () => {
    asCaller(null)
    mockApi.get.mockRejectedValue(
      Object.assign(new Error('Missing MANAGE_AGENTS permission'), { status: 403 }),
    )
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(TENANT_ID)

    expect(mockApi.get).toHaveBeenCalledTimes(1)
    expect(store.refused).toBe(true)
    expect(store.error).toBeNull()
    expect(store.policies).toEqual([])
  })

  it('any other failure is an error, not a refusal', async () => {
    asCaller(MANAGE_AGENTS)
    mockApi.get.mockRejectedValue(Object.assign(new Error('API error 500'), { status: 500 }))
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(TENANT_ID)

    expect(store.error).toBe('API error 500')
    expect(store.refused).toBe(false)
    expect(store.policies).toEqual([])
  })

  it('a later fetch by a holder clears an earlier refusal', async () => {
    asCaller(VIEW_CHANNELS)
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(TENANT_ID)
    expect(store.refused).toBe(true)

    asCaller(MANAGE_AGENTS)
    mockApi.get.mockResolvedValue(listOf([mkPolicy()]))
    await store.fetchPolicies(TENANT_ID)
    expect(store.refused).toBe(false)
    expect(store.policies).toHaveLength(1)
  })
})

// #4188 — the route answers one page at a time: 25 rows unless the request
// asks for more, and never more than 100.
describe('tunnelPolicies store: an org with more policies than one page holds', () => {
  const named = (i: number) => mkPolicy({ id: rowId(i), name: `policy-${i}` })

  it('lists every policy, in the server order, each once, with the server total', async () => {
    asCaller(MANAGE_AGENTS)
    mockApi.get.mockImplementation(pagedRoute({ total: 101, row: named, totalPages: true }))
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(TENANT_ID)

    expect(store.error).toBeNull()
    expect(store.refused).toBe(false)
    expect(store.policies).toHaveLength(101)
    expect(store.policies.map((p) => p.name)).toEqual(
      Array.from({ length: 101 }, (_, i) => `policy-${i}`),
    )
    expect(store.total).toBe(101)
  })

  it('stops at the page cap, keeps what it read, and says the cap cut the list', async () => {
    asCaller(MANAGE_AGENTS)
    mockApi.get.mockImplementation(pagedRoute({ total: 5001, row: named, totalPages: true }))
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(TENANT_ID)

    expect(mockApi.get).toHaveBeenCalledTimes(50)
    expect(store.policies).toHaveLength(5000)
    expect(store.total).toBe(5001)
    expect(store.truncated).toBe(true)
    expect(store.error).toBeNull()

    // A later read that gets the whole list clears it.
    mockApi.get.mockImplementation(pagedRoute({ total: 3, row: named, totalPages: true }))
    await store.fetchPolicies(TENANT_ID)
    expect(store.policies).toHaveLength(3)
    expect(store.truncated).toBe(false)
  })

  it('stops at a failed page: the error shows, and no part of the list is kept', async () => {
    asCaller(MANAGE_AGENTS)
    const failure = Object.assign(new Error('API error 500'), { status: 500 })
    mockApi.get.mockImplementation(
      pagedRoute({ total: 250, row: named, totalPages: true, fail: { page: 2, error: failure } }),
    )
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(TENANT_ID)

    expect(mockApi.get).toHaveBeenCalledTimes(2)
    expect(store.error).toBe('API error 500')
    expect(store.refused).toBe(false)
    expect(store.policies).toEqual([])
    expect(store.total).toBe(0)
    expect(store.truncated).toBe(false)
    expect(store.loading).toBe(false)
  })

  it('a 403 on a later page is the same refusal as on the first', async () => {
    asCaller(MANAGE_AGENTS)
    const refusal = Object.assign(new Error('Missing MANAGE_AGENTS permission'), { status: 403 })
    mockApi.get.mockImplementation(
      pagedRoute({ total: 250, row: named, totalPages: true, fail: { page: 2, error: refusal } }),
    )
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(TENANT_ID)

    expect(mockApi.get).toHaveBeenCalledTimes(2)
    expect(store.refused).toBe(true)
    expect(store.error).toBeNull()
    expect(store.policies).toEqual([])
    expect(store.total).toBe(0)
  })
})

// #4248 — the store holds one org's policies at a time, and knows which.
// TENANT_ID is org A.
describe('tunnelPolicies store: one org at a time (#4248)', () => {
  const ORG_B = '69a1dbbad2000f26adc875cf'
  const listUrl = (tenantId: string) => `/tenant/${tenantId}/tunnel-policy?page=1&per_page=100`

  it("org A's late refusal neither refuses nor empties org B's tab", async () => {
    // Org A's mask could not be read, so its list is asked and the server
    // refuses it; in org B the caller holds MANAGE_AGENTS.
    asCaller(null)
    let refuseA!: (e: unknown) => void
    mockApi.get.mockImplementation(async (path: string): Promise<never> => {
      if (path === listUrl(TENANT_ID)) {
        return new Promise<never>((_, reject) => {
          refuseA = reject
        })
      }
      if (path === `/tenant/${ORG_B}/member/me`) {
        return { permissions: MANAGE_AGENTS, is_owner: false } as never
      }
      if (path === listUrl(ORG_B)) {
        return listOf([mkPolicy({ id: 'pb', tenant_id: ORG_B })]) as never
      }
      throw new Error(`unexpected GET ${path}`)
    })
    const store = useTunnelPolicyStore()
    const loadA = store.fetchPolicies(TENANT_ID)
    await vi.waitFor(() => expect(mockApi.get).toHaveBeenCalledWith(listUrl(TENANT_ID)))
    // Org A's list is still out: org A's tab shows nothing of org B's yet.
    expect(store.policies).toEqual([])

    await store.fetchPolicies(ORG_B)
    refuseA(Object.assign(new Error('Missing MANAGE_AGENTS permission'), { status: 403 }))
    await loadA

    expect(mockApi.get.mock.calls.map((c) => c[0])).toEqual([
      listUrl(TENANT_ID),
      `/tenant/${ORG_B}/member/me`,
      listUrl(ORG_B),
    ])
    expect(store.refused).toBe(false)
    expect(store.error).toBeNull()
    expect(store.policies.map((p) => p.id)).toEqual(['pb'])
    expect(store.total).toBe(1)
    expect(store.policiesTenantId).toBe(ORG_B)
    expect(store.loading).toBe(false)
  })

  it("a fetch for org B drops org A's policies at once", async () => {
    asCaller(MANAGE_AGENTS)
    mockApi.get.mockResolvedValueOnce(listOf([mkPolicy({ id: 'pa' })]))
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(TENANT_ID)
    expect(store.policies.map((p) => p.id)).toEqual(['pa'])

    mockApi.get.mockReturnValue(new Promise(() => {}))
    void store.fetchPolicies(ORG_B)
    expect(store.policies).toEqual([])
    expect(store.total).toBe(0)
    expect(store.policiesTenantId).toBe(ORG_B)
  })

  it("a save, an edit or a delete answered for org A leaves org B's list as it is", async () => {
    const tenants = useTenantStore()
    tenants.isOwner = false
    tenants.myPermissions = MANAGE_AGENTS
    tenants.myPermissionsTenantId = ORG_B
    mockApi.get.mockResolvedValueOnce(listOf([mkPolicy({ id: 'pb', tenant_id: ORG_B })]))
    const store = useTunnelPolicyStore()
    await store.fetchPolicies(ORG_B)
    const { name, subjects, targets, allowlist } = mkPolicy()

    mockApi.post.mockResolvedValueOnce(mkPolicy({ id: 'pa' }))
    await store.createPolicy(TENANT_ID, { name, subjects, targets, allowlist })
    mockApi.put.mockResolvedValueOnce(mkPolicy({ id: 'pb', name: 'renamed in org A' }))
    await store.updatePolicy(TENANT_ID, 'pb', { name: 'renamed in org A' })
    mockApi.delete.mockResolvedValueOnce({ deleted: true })
    await store.deletePolicy(TENANT_ID, 'pb')

    expect(store.policies.map((p) => p.id)).toEqual(['pb'])
    expect(store.policies[0].name).toBe(name)
    expect(store.total).toBe(1)
  })
})
