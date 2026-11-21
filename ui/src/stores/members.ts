// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

/** A tenant member row from `GET /tenant/{id}/member` (paginated). */
export interface Member {
  id: string
  user_id: string
  nickname?: string | null
  display_name: string
  /** FR-11: org members see each other's addresses — that is what the
   *  members page is for. Empty when the user row is gone (defensive). */
  email: string
  role_ids: string[]
  joined_at: string
}

interface MembersPage {
  items: Member[]
  total: number
  page: number
  per_page: number
  total_pages: number
}

/** FR-11 grid params — mirrors the server's flat MemberListQuery. */
export interface MemberFetchOpts {
  page?: number
  perPage?: number
  q?: string
  /** Server whitelist: name | email | joined_at (absent = joined_at asc). */
  sort?: string
  dir?: 'asc' | 'desc'
}

export const useMembersStore = defineStore('members', () => {
  const items = ref<Member[]>([])
  const total = ref(0)
  const page = ref(1)
  const perPage = ref(25)
  const totalPages = ref(1)
  const loading = ref(false)
  const error = ref<string | null>(null)
  /** #4248 — which org `items` describes: the org of the latest fetch,
   *  answered or not. A fetch for another org drops the rows first, so one
   *  org's people never show (or fill a picker) under another org's page. */
  const membersTenantId = ref<string | null>(null)

  // Stale-response guard: a slow page-1 response must not clobber a fast
  // page-2 one (same pattern as the devices grid store), nor a slow answer
  // for the org asked before it the new org's (#4248).
  let seq = 0

  async function fetchMembers(tenantId: string, opts: MemberFetchOpts | number = {}) {
    // Back-compat: the pre-FR-11 signature was (tenantId, page).
    const o: MemberFetchOpts = typeof opts === 'number' ? { page: opts } : opts
    const mySeq = ++seq
    if (membersTenantId.value !== null && membersTenantId.value !== tenantId) {
      items.value = []
      total.value = 0
    }
    membersTenantId.value = tenantId
    loading.value = true
    error.value = null
    try {
      const params = new URLSearchParams()
      params.set('page', String(o.page ?? 1))
      params.set('per_page', String(o.perPage ?? perPage.value))
      if (o.q) params.set('q', o.q)
      if (o.sort) {
        params.set('sort', o.sort)
        if (o.dir) params.set('dir', o.dir)
      }
      const resp = await api.get<MembersPage>(`/tenant/${tenantId}/member?${params.toString()}`)
      if (mySeq !== seq) return
      items.value = resp.items
      total.value = resp.total
      page.value = resp.page
      perPage.value = resp.per_page
      totalPages.value = resp.total_pages
    } catch (e) {
      if (mySeq !== seq) return
      error.value = (e as Error).message
      items.value = []
    } finally {
      if (mySeq === seq) loading.value = false
    }
  }

  /**
   * FR-11: add an existing account directly by email (no invite round-trip).
   * The server resolves only PROVEN addresses; unknown → 404 whose message
   * points the admin at Invites. Caller refetches on success.
   */
  async function addByEmail(tenantId: string, email: string, roleIds: string[] = []) {
    return api.post<{ id: string; user_id: string; tenant_id: string }>(
      `/tenant/${tenantId}/member`,
      { email, role_ids: roleIds },
    )
  }

  /**
   * FR-11: remove a member (KICK_MEMBERS server-side; the tenant owner is
   * unremovable — 409). Caller refetches on success.
   */
  async function removeMember(tenantId: string, userId: string) {
    return api.delete<{ removed: boolean }>(`/tenant/${tenantId}/member/${userId}`)
  }

  /**
   * Local mirror of a server-side role assign/unassign — the role store owns
   * the API calls; this keeps the loaded member rows honest without a refetch.
   */
  function setMemberRole(userId: string, roleId: string, present: boolean) {
    const member = items.value.find((m) => m.user_id === userId)
    if (!member) return
    const has = member.role_ids.includes(roleId)
    if (present && !has) member.role_ids.push(roleId)
    if (!present && has) member.role_ids = member.role_ids.filter((r) => r !== roleId)
  }

  return {
    items,
    total,
    page,
    perPage,
    totalPages,
    loading,
    error,
    membersTenantId,
    fetchMembers,
    addByEmail,
    removeMember,
    setMemberRole,
  }
})
