// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

interface InviteInfo {
  code: string
  tenant_id?: string
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
  /** Set by create/batch responses: whether THIS request dispatched an
   *  email. Optional — absent on old servers and on list responses. */
  email_sent?: boolean
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

interface BatchInviteResult {
  invite?: Invite
  error?: string
  target_email?: string
}

interface BatchCreateResponse {
  results: BatchInviteResult[]
  created: number
  failed: number
}

/** FR-11 grid params — mirrors the server's flat InviteListQuery. */
export interface InviteFetchOpts {
  page?: number
  perPage?: number
  q?: string
  /** Exact filter: active | expired | revoked | exhausted. */
  status?: string
  /** Server whitelist: created_at | target_email | status. */
  sort?: string
  dir?: 'asc' | 'desc'
}

export const useInviteStore = defineStore('invite', () => {
  const inviteInfo = ref<InviteInfo | null>(null)
  const invites = ref<Invite[]>([])
  const total = ref(0)
  const page = ref(1)
  const perPage = ref(25)
  const totalPages = ref(1)
  // Stale-response guard (devices-grid pattern): the LAST issued list wins.
  let listSeq = 0
  const loading = ref(false)
  const error = ref<string | null>(null)
  /** #4257 — which org `invites` describes: the org of the latest list,
   *  answered or not. A list for another org drops the rows first, so one
   *  org's invites never show under another org's page, while that org's
   *  list loads or after it failed. */
  const invitesTenantId = ref<string | null>(null)

  /** An invite created for `tenantId` joins the list only when the list is
   *  not another org's. */
  const listIsOf = (tenantId: string) =>
    invitesTenantId.value === null || invitesTenantId.value === tenantId

  /** #4257 — the latest list fetch is in flight, and the spinner is its own: a
   *  create or a batch never ends it. A write answered for an org the list is
   *  not of leaves the spinner and the error alone (the page shows them above
   *  another org's grid). */
  let listInFlight = false

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

  async function listInvites(tenantId: string, opts: InviteFetchOpts | number = {}) {
    // Back-compat: the pre-FR-11 signature was (tenantId, page).
    const o: InviteFetchOpts = typeof opts === 'number' ? { page: opts } : opts
    const mySeq = ++listSeq
    if (invitesTenantId.value !== null && invitesTenantId.value !== tenantId) {
      invites.value = []
      total.value = 0
    }
    invitesTenantId.value = tenantId
    loading.value = true
    listInFlight = true
    error.value = null
    try {
      const params = new URLSearchParams()
      params.set('page', String(o.page ?? 1))
      params.set('per_page', String(o.perPage ?? perPage.value))
      if (o.q) params.set('q', o.q)
      if (o.status) params.set('status', o.status)
      if (o.sort) {
        params.set('sort', o.sort)
        if (o.dir) params.set('dir', o.dir)
      }
      const data = await api.get<InviteList>(
        `/tenant/${tenantId}/invite?${params.toString()}`,
      )
      if (mySeq !== listSeq) return
      invites.value = data.items
      total.value = data.total
      page.value = data.page
      perPage.value = data.per_page
      totalPages.value = data.total_pages
    } catch (e) {
      if (mySeq !== listSeq) return
      error.value = (e as Error).message
      throw e
    } finally {
      if (mySeq === listSeq) {
        loading.value = false
        listInFlight = false
      }
    }
  }

  async function createInvite(tenantId: string, params: CreateInviteParams): Promise<Invite> {
    loading.value = true
    error.value = null
    try {
      const invite = await api.post<Invite>(`/tenant/${tenantId}/invite`, params)
      // #4257 — a create answered after the page moved on to another org
      // happened all the same; it stays out of that org's list.
      if (listIsOf(tenantId)) invites.value.unshift(invite)
      return invite
    } catch (e) {
      if (listIsOf(tenantId)) error.value = (e as Error).message
      throw e
    } finally {
      if (listIsOf(tenantId) && !listInFlight) loading.value = false
    }
  }

  async function batchCreateInvites(
    tenantId: string,
    params: CreateInviteParams[],
  ): Promise<BatchCreateResponse> {
    loading.value = true
    error.value = null
    try {
      const result = await api.post<BatchCreateResponse>(
        `/tenant/${tenantId}/invite/batch`,
        { invites: params },
      )
      // Add successfully created invites to the list, when it is this org's
      // (#4257: a batch answered for an org the page has left stays out).
      for (const r of result.results) {
        if (r.invite && listIsOf(tenantId)) {
          invites.value.unshift(r.invite)
        }
      }
      return result
    } catch (e) {
      if (listIsOf(tenantId)) error.value = (e as Error).message
      throw e
    } finally {
      if (listIsOf(tenantId) && !listInFlight) loading.value = false
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
    invitesTenantId,
    total,
    page,
    perPage,
    totalPages,
    loading,
    error,
    fetchInviteInfo,
    acceptInvite,
    listInvites,
    createInvite,
    batchCreateInvites,
    revokeInvite,
  }
})
