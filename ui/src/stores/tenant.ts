import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

interface TenantBilling {
  status?: string
  cancel_at_period_end?: boolean
  current_period_end?: string | number | Date
  customer_id?: string
}

interface Tenant {
  id: string
  name: string
  slug: string
  description?: string
  icon?: string
  plan?: string
  billing?: TenantBilling
}

interface MyMembership {
  permissions: number
  is_owner: boolean
}

export const useTenantStore = defineStore('tenant', () => {
  const tenants = ref<Tenant[]>([])
  const current = ref<Tenant | null>(null)
  const loading = ref(false)
  /** The caller's permission mask in the ACTIVE tenant (null until the
   *  /member/me fetch lands — consumers must fail open on null). */
  const myPermissions = ref<number | null>(null)
  const isOwner = ref(false)

  /**
   * Load the caller's own membership for `tenantId`. Resets to the
   * unknown state first so a tenant switch never shows the previous
   * tenant's mask. 403/404/older-server → stays null (fail-open).
   */
  async function fetchMyMembership(tenantId: string) {
    myPermissions.value = null
    isOwner.value = false
    try {
      const m = await api.get<MyMembership>(`/tenant/${tenantId}/member/me`)
      myPermissions.value = m.permissions
      isOwner.value = m.is_owner
    } catch {
      /* fail open — nav gating treats null as "show" */
    }
  }

  async function fetchTenants() {
    loading.value = true
    try {
      tenants.value = await api.get<Tenant[]>('/tenant')
      if (!current.value && tenants.value.length > 0) {
        current.value = tenants.value[0]!
      }
    } finally {
      loading.value = false
    }
  }

  async function createTenant(name: string, slug: string) {
    const tenant = await api.post<Tenant>('/tenant', { name, slug })
    tenants.value.push(tenant)
    current.value = tenant
    return tenant
  }

  function setCurrent(tenant: Tenant) {
    current.value = tenant
  }

  return {
    tenants,
    current,
    loading,
    myPermissions,
    isOwner,
    fetchTenants,
    fetchMyMembership,
    createTenant,
    setCurrent,
  }
})
