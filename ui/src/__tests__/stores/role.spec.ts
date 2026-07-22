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
