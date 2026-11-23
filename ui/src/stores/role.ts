// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
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
  /** #4248 — which org `roles` describes: the org of the latest fetch,
   *  answered or not. A fetch for another org drops the rows first, so one
   *  org's roles never show (or fill a picker) under another org's page. */
  const rolesTenantId = ref<string | null>(null)
  // Only the latest fetch writes: the answer for an org asked before it (the
  // page moved on to another org) is dropped.
  let seq = 0
  // The latest fetch failed: `ensureRoles` asks again.
  let failed = false

  async function fetchRoles(tenantId: string) {
    const mySeq = ++seq
    if (rolesTenantId.value !== null && rolesTenantId.value !== tenantId) roles.value = []
    rolesTenantId.value = tenantId
    failed = false
    loading.value = true
    error.value = null
    try {
      const list = await api.get<Role[]>(`/tenant/${tenantId}/role`)
      if (mySeq !== seq) return
      roles.value = list
    } catch (e) {
      if (mySeq !== seq) return
      failed = true
      error.value = (e as Error).message
      roles.value = []
    } finally {
      if (mySeq === seq) loading.value = false
    }
  }

  /** #4257 — the org's roles for a dialog or a list that only names or picks
   *  them, fetched only when the store does not hold them already. A store
   *  that holds another org's roles, or whose last fetch failed, fetches; one
   *  that holds this org's (or is loading them) does not. It replaces a check
   *  of an empty list, which never reloaded a list holding another org's. */
  function ensureRoles(tenantId: string): Promise<void> {
    if (rolesTenantId.value === tenantId && !failed) return Promise.resolve()
    return fetchRoles(tenantId)
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
    rolesTenantId,
    fetchRoles,
    ensureRoles,
    createRole,
    updateRole,
    deleteRole,
    assignRole,
    unassignRole,
    colorHex,
  }
})
