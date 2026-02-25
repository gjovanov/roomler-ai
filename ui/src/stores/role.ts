import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

export interface Role {
  id: string
  tenant_id: string
  name: string
  description?: string
  color?: number
  position: number
  permissions: number
  is_default: boolean
  is_managed: boolean
  is_mentionable: boolean
}

export const useRoleStore = defineStore('roles', () => {
  const roles = ref<Role[]>([])
  const loading = ref(false)

  async function fetchRoles(tenantId: string) {
    loading.value = true
    try {
      roles.value = await api.get<Role[]>(`/tenant/${tenantId}/role`)
    } finally {
      loading.value = false
    }
  }

  async function createRole(tenantId: string, payload: Partial<Role>) {
    const role = await api.post<Role>(`/tenant/${tenantId}/role`, payload)
    roles.value.push(role)
    return role
  }

  async function updateRole(tenantId: string, roleId: string, payload: Partial<Role>) {
    await api.put(`/tenant/${tenantId}/role/${roleId}`, payload)
    const idx = roles.value.findIndex((r) => r.id === roleId)
    if (idx !== -1) {
      Object.assign(roles.value[idx], payload)
    }
  }

  async function deleteRole(tenantId: string, roleId: string) {
    await api.delete(`/tenant/${tenantId}/role/${roleId}`)
    roles.value = roles.value.filter((r) => r.id !== roleId)
  }

  async function assignRole(tenantId: string, roleId: string, userId: string) {
    await api.post(`/tenant/${tenantId}/role/${roleId}/assign/${userId}`)
  }

  async function unassignRole(tenantId: string, roleId: string, userId: string) {
    await api.delete(`/tenant/${tenantId}/role/${roleId}/assign/${userId}`)
  }

  function colorHex(color?: number): string | undefined {
    if (!color) return undefined
    return `#${color.toString(16).padStart(6, '0')}`
  }

  return {
    roles,
    loading,
    fetchRoles,
    createRole,
    updateRole,
    deleteRole,
    assignRole,
    unassignRole,
    colorHex,
  }
})
