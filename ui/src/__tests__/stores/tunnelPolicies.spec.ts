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

    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/tunnel-policy`)
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
