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

export const useTenantStore = defineStore('tenant', () => {
  const tenants = ref<Tenant[]>([])
  const current = ref<Tenant | null>(null)
  const loading = ref(false)

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

  return { tenants, current, loading, fetchTenants, createTenant, setCurrent }
})
