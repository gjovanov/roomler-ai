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
  // List-level error surface (sibling-store convention); dialog-level save
  // errors stay local to the section so the dialog can stay open.
  const error = ref<string | null>(null)

  async function fetchRoles(tenantId: string) {
    loading.value = true
    error.value = null
    try {
      roles.value = await api.get<Role[]>(`/tenant/${tenantId}/role`)
    } catch (e) {
      error.value = (e as Error).message
      roles.value = []
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
      // Only mirror keys the server actually received: an `undefined` value
      // is dropped from the JSON body (field left unchanged server-side), so
      // copying it locally would fake a change that never happened.
      const defined = Object.fromEntries(
        Object.entries(payload).filter(([, v]) => v !== undefined),
      )
      Object.assign(roles.value[idx], defined)
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
    error,
    fetchRoles,
    createRole,
    updateRole,
    deleteRole,
    assignRole,
    unassignRole,
    colorHex,
  }
})
