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

import { useTenantStore } from '@/stores/tenant'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

describe('useTenantStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  describe('initial state', () => {
    it('should start with empty tenants and no current', () => {
      const store = useTenantStore()
      expect(store.tenants).toEqual([])
      expect(store.current).toBeNull()
      expect(store.loading).toBe(false)
    })
  })

  describe('fetchTenants', () => {
    it('should fetch and store tenants', async () => {
      const tenants = [
        { id: 't1', name: 'Tenant 1', slug: 'tenant-1' },
        { id: 't2', name: 'Tenant 2', slug: 'tenant-2' },
      ]
      mockApi.get.mockResolvedValueOnce(tenants)

      const store = useTenantStore()
      await store.fetchTenants()

      expect(mockApi.get).toHaveBeenCalledWith('/tenant')
      expect(store.tenants).toEqual(tenants)
    })

    it('should auto-select first tenant as current when none is set', async () => {
      const tenants = [
        { id: 't1', name: 'Tenant 1', slug: 'tenant-1' },
        { id: 't2', name: 'Tenant 2', slug: 'tenant-2' },
      ]
      mockApi.get.mockResolvedValueOnce(tenants)

      const store = useTenantStore()
      await store.fetchTenants()

      expect(store.current).toEqual(tenants[0])
    })

    it('should not overwrite current tenant if already set', async () => {
      const existingTenant = { id: 't2', name: 'Tenant 2', slug: 'tenant-2' }
      const tenants = [
        { id: 't1', name: 'Tenant 1', slug: 'tenant-1' },
        existingTenant,
      ]
      mockApi.get.mockResolvedValueOnce(tenants)

      const store = useTenantStore()
      store.current = existingTenant
      await store.fetchTenants()

      expect(store.current).toEqual(existingTenant)
    })

    it('should not set current when no tenants returned', async () => {
      mockApi.get.mockResolvedValueOnce([])

      const store = useTenantStore()
      await store.fetchTenants()

      expect(store.current).toBeNull()
    })

    it('should set loading while fetching', async () => {
      let resolvePromise: (v: unknown) => void
      mockApi.get.mockReturnValueOnce(new Promise((r) => { resolvePromise = r }))

      const store = useTenantStore()
      const promise = store.fetchTenants()
      expect(store.loading).toBe(true)

      resolvePromise!([])
      await promise
      expect(store.loading).toBe(false)
    })

    it('should reset loading on error', async () => {
      mockApi.get.mockRejectedValueOnce(new Error('fail'))

      const store = useTenantStore()
      await expect(store.fetchTenants()).rejects.toThrow()
      expect(store.loading).toBe(false)
    })
  })

  describe('createTenant', () => {
    it('should create tenant, add to list, and set as current', async () => {
      const newTenant = { id: 't3', name: 'New Tenant', slug: 'new-tenant' }
      mockApi.post.mockResolvedValueOnce(newTenant)

      const store = useTenantStore()
      const result = await store.createTenant('New Tenant', 'new-tenant')

      expect(mockApi.post).toHaveBeenCalledWith('/tenant', { name: 'New Tenant', slug: 'new-tenant' })
      expect(result).toEqual(newTenant)
      expect(store.tenants).toContainEqual(newTenant)
      expect(store.current).toEqual(newTenant)
    })

    it('should append to existing tenants', async () => {
      const existing = { id: 't1', name: 'Existing', slug: 'existing' }
      const newTenant = { id: 't2', name: 'New', slug: 'new' }
      mockApi.post.mockResolvedValueOnce(newTenant)

      const store = useTenantStore()
      store.tenants = [existing]
      await store.createTenant('New', 'new')

      expect(store.tenants).toHaveLength(2)
      expect(store.tenants[0]).toEqual(existing)
      expect(store.tenants[1]).toEqual(newTenant)
    })
  })

  describe('setCurrent', () => {
    it('should set the current tenant', () => {
      const store = useTenantStore()
      const tenant = { id: 't1', name: 'Tenant', slug: 'tenant' }
      store.setCurrent(tenant)
      expect(store.current).toEqual(tenant)
    })

    it('should replace existing current tenant', () => {
      const store = useTenantStore()
      store.current = { id: 't1', name: 'Old', slug: 'old' }
      const newTenant = { id: 't2', name: 'New', slug: 'new' }
      store.setCurrent(newTenant)
      expect(store.current).toEqual(newTenant)
    })
  })
})
