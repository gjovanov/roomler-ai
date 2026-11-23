// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import { useRoleStore, type Role } from '@/stores/role'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

const TENANT_ID = '69a1dbbad2000f26adc875ff'

function mkRole(over: Partial<Role> = {}): Role {
  return {
    id: 'r1',
    tenant_id: TENANT_ID,
    name: 'Operator',
    description: 'Remote operators',
    color: 0x1976d2,
    position: 10,
    permissions: (1 << 24) | (1 << 25),
    is_default: false,
    is_managed: false,
    is_mentionable: true,
    ...over,
  }
}

beforeEach(() => {
  setActivePinia(createPinia())
  vi.clearAllMocks()
})

describe('role store', () => {
  it('fetchRoles loads the list and clears errors', async () => {
    const store = useRoleStore()
    mockApi.get.mockResolvedValueOnce([mkRole()])

    await store.fetchRoles(TENANT_ID)

    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/role`)
    expect(store.roles).toHaveLength(1)
    expect(store.roles[0].name).toBe('Operator')
    expect(store.error).toBeNull()
    expect(store.loading).toBe(false)
  })

  it('fetchRoles failure surfaces the error and empties the list', async () => {
    const store = useRoleStore()
    store.roles = [mkRole()]
    mockApi.get.mockRejectedValueOnce(new Error('Missing MANAGE_ROLES permission'))

    await store.fetchRoles(TENANT_ID)

    expect(store.error).toContain('MANAGE_ROLES')
    expect(store.roles).toEqual([])
  })

  it('createRole posts and appends', async () => {
    const store = useRoleStore()
    const created = mkRole({ id: 'r2', name: 'Auditors' })
    mockApi.post.mockResolvedValueOnce(created)

    const out = await store.createRole(TENANT_ID, { name: 'Auditors' })

    expect(mockApi.post).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/role`, { name: 'Auditors' })
    expect(out.id).toBe('r2')
    expect(store.roles.map((r) => r.id)).toContain('r2')
  })

  it('updateRole puts and merges locally', async () => {
    const store = useRoleStore()
    store.roles = [mkRole()]
    mockApi.put.mockResolvedValueOnce({ updated: true })

    await store.updateRole(TENANT_ID, 'r1', { name: 'Ops', permissions: 1 << 25 })

    expect(mockApi.put).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/role/r1`, {
      name: 'Ops',
      permissions: 1 << 25,
    })
    expect(store.roles[0].name).toBe('Ops')
    expect(store.roles[0].permissions).toBe(1 << 25)
  })

  it('updateRole does not mirror undefined keys locally (omitted = unchanged server-side)', async () => {
    const store = useRoleStore()
    store.roles = [mkRole({ color: 0x1976d2 })]
    mockApi.put.mockResolvedValueOnce({ updated: true })

    await store.updateRole(TENANT_ID, 'r1', { name: 'Ops', color: undefined })

    expect(store.roles[0].color).toBe(0x1976d2)
    expect(store.roles[0].name).toBe('Ops')
  })

  it('deleteRole deletes and drops locally', async () => {
    const store = useRoleStore()
    store.roles = [mkRole(), mkRole({ id: 'r2' })]
    mockApi.delete.mockResolvedValueOnce({ deleted: true })

    await store.deleteRole(TENANT_ID, 'r1')

    expect(mockApi.delete).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/role/r1`)
    expect(store.roles.map((r) => r.id)).toEqual(['r2'])
  })

  it('assign/unassign hit the member-scoped endpoints', async () => {
    const store = useRoleStore()
    mockApi.post.mockResolvedValueOnce({ assigned: true })
    mockApi.delete.mockResolvedValueOnce({ removed: true })

    await store.assignRole(TENANT_ID, 'r1', 'u1')
    await store.unassignRole(TENANT_ID, 'r1', 'u1')

    expect(mockApi.post).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/role/r1/assign/u1`)
    expect(mockApi.delete).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/role/r1/assign/u1`)
  })

  it('colorHex renders u32 as #rrggbb and passes through undefined', () => {
    const store = useRoleStore()
    expect(store.colorHex(0x1976d2)).toBe('#1976d2')
    expect(store.colorHex(0x00000f)).toBe('#00000f')
    expect(store.colorHex(undefined)).toBeUndefined()
  })
})

// #4248 — the store holds one org's roles at a time, and knows which.
describe('role store: one org at a time (#4248)', () => {
  const ORG_A = 'org_a'
  const ORG_B = 'org_b'

  it("a fetch for org B drops org A's roles at once; a refresh of org A keeps them while it loads", async () => {
    const store = useRoleStore()
    mockApi.get.mockResolvedValueOnce([mkRole({ id: 'ra' })])
    await store.fetchRoles(ORG_A)

    let again!: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => (again = r)))
    const refreshA = store.fetchRoles(ORG_A)
    expect(store.roles.map((r) => r.id)).toEqual(['ra'])
    again([mkRole({ id: 'ra' })])
    await refreshA

    mockApi.get.mockReturnValueOnce(new Promise(() => {}))
    void store.fetchRoles(ORG_B)
    expect(store.roles).toEqual([])
    expect(store.rolesTenantId).toBe(ORG_B)
  })

  it("org A's late failure lands on nothing: org B's roles stay, with no error", async () => {
    const store = useRoleStore()
    let failA!: (e: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((_, reject) => (failA = reject)))
    const loadA = store.fetchRoles(ORG_A)
    mockApi.get.mockResolvedValueOnce([mkRole({ id: 'rb', tenant_id: ORG_B })])
    await store.fetchRoles(ORG_B)

    failA(new Error('Missing MANAGE_ROLES permission'))
    await loadA
    expect(store.roles.map((r) => r.id)).toEqual(['rb'])
    expect(store.error).toBeNull()
    expect(store.loading).toBe(false)
  })
})

// #4257 — a dialog or a list that only names or picks the org's roles asks
// with `ensureRoles`, which fetches only when the store does not hold them.
describe('role store: ensureRoles (#4257)', () => {
  const ORG_A = 'org_a'
  const ORG_B = 'org_b'
  const asked = () => mockApi.get.mock.calls.map((c) => c[0])

  // A test that stops before its held GET is sent must not hand it to the next.
  beforeEach(() => mockApi.get.mockReset())

  it("asked for org B while holding org A's: one fetch, for org B", async () => {
    const store = useRoleStore()
    mockApi.get.mockResolvedValueOnce([mkRole({ id: 'ra' })])
    await store.fetchRoles(ORG_A)
    mockApi.get.mockResolvedValueOnce([mkRole({ id: 'rb', tenant_id: ORG_B })])
    await store.ensureRoles(ORG_B)
    expect(asked()).toEqual([`/tenant/${ORG_A}/role`, `/tenant/${ORG_B}/role`])
    expect(store.roles.map((r) => r.id)).toEqual(['rb'])
  })

  it("asked for org A while holding org A's, or loading them: no fetch, for an org with none too", async () => {
    const store = useRoleStore()
    let answer!: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => (answer = r)))
    const first = store.fetchRoles(ORG_A)
    await store.ensureRoles(ORG_A)
    answer([])
    await first
    await store.ensureRoles(ORG_A)
    expect(asked()).toEqual([`/tenant/${ORG_A}/role`])
  })

  it('asked again after a failed fetch: fetches', async () => {
    const store = useRoleStore()
    mockApi.get.mockRejectedValueOnce(new Error('network'))
    await store.fetchRoles(ORG_A)
    mockApi.get.mockResolvedValueOnce([mkRole({ id: 'ra' })])
    await store.ensureRoles(ORG_A)
    expect(asked()).toHaveLength(2)
    expect(store.roles.map((r) => r.id)).toEqual(['ra'])
    expect(store.error).toBeNull()
  })
})
