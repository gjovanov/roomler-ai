import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

interface InviteInfo {
  code: string
  tenant_name: string
  tenant_slug: string
  inviter_name: string
  is_valid: boolean
  status: string
  already_member?: boolean
}

interface Invite {
  id: string
  code: string
  tenant_id: string
  inviter_id: string
  target_email?: string
  max_uses?: number
  use_count: number
  status: string
  assign_role_ids: string[]
  expires_at?: string
  created_at: string
}

interface InviteList {
  items: Invite[]
  total: number
  page: number
  per_page: number
  total_pages: number
}

interface AcceptResult {
  tenant_id: string
  tenant_name: string
  tenant_slug: string
}

interface CreateInviteParams {
  target_email?: string
  max_uses?: number
  expires_in_hours?: number
  assign_role_ids?: string[]
}

export const useInviteStore = defineStore('invite', () => {
  const inviteInfo = ref<InviteInfo | null>(null)
  const invites = ref<Invite[]>([])
  const total = ref(0)
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function fetchInviteInfo(code: string) {
    loading.value = true
    error.value = null
    try {
      inviteInfo.value = await api.get<InviteInfo>(`/invite/${code}`)
    } catch (e) {
      error.value = (e as Error).message
      throw e
    } finally {
      loading.value = false
    }
  }

  async function acceptInvite(code: string): Promise<AcceptResult> {
    loading.value = true
    error.value = null
    try {
      return await api.post<AcceptResult>(`/invite/${code}/accept`)
    } catch (e) {
      error.value = (e as Error).message
      throw e
    } finally {
      loading.value = false
    }
  }

  async function listInvites(tenantId: string, page = 1) {
    loading.value = true
    error.value = null
    try {
      const data = await api.get<InviteList>(
        `/tenant/${tenantId}/invite?page=${page}`,
      )
      invites.value = data.items
      total.value = data.total
    } catch (e) {
      error.value = (e as Error).message
      throw e
    } finally {
      loading.value = false
    }
  }

  async function createInvite(tenantId: string, params: CreateInviteParams): Promise<Invite> {
    loading.value = true
    error.value = null
    try {
      const invite = await api.post<Invite>(`/tenant/${tenantId}/invite`, params)
      invites.value.unshift(invite)
      return invite
    } catch (e) {
      error.value = (e as Error).message
      throw e
    } finally {
      loading.value = false
    }
  }

  async function revokeInvite(tenantId: string, inviteId: string) {
    loading.value = true
    error.value = null
    try {
      await api.delete(`/tenant/${tenantId}/invite/${inviteId}`)
      const idx = invites.value.findIndex((i) => i.id === inviteId)
      if (idx !== -1) invites.value[idx].status = 'revoked'
    } catch (e) {
      error.value = (e as Error).message
      throw e
    } finally {
      loading.value = false
    }
  }

  return {
    inviteInfo,
    invites,
    total,
    loading,
    error,
    fetchInviteInfo,
    acceptInvite,
    listInvites,
    createInvite,
    revokeInvite,
  }
})
