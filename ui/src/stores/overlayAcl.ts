// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'
import { fetchAllPages } from '@/utils/allPages'

// Matches `crates/remote_control/src/models.rs` (OverlaySelector / OverlayTarget
// / OverlayRule / OverlayAclMode) and the answers of
// `crates/modules/network/src/routes/overlay_policy.rs`: snake_case `kind`-tagged
// unions as `{kind, id}`, every id its 24-hex string (#3636), sent back as it is.

/** Who the rule applies to — the node ORIGINATING traffic. */
export type OverlaySelector =
  | { kind: 'all_nodes' }
  | { kind: 'node_id'; id: string }
  | { kind: 'user_id'; id: string }
  | { kind: 'role_id'; id: string }

/** Which node they may reach (and, if it routes subnets, through). */
export type OverlayTarget = { kind: 'all_nodes' } | { kind: 'node_id'; id: string }

export interface OverlayRule {
  cidr: string
  port_range: { low: number; high: number }
  proto: 'tcp' | 'udp' | 'any'
}

/**
 * Tenant-wide posture. `off` keeps the pre-ACL behaviour (every node sees every
 * peer); `warn` evaluates and logs what WOULD be denied without changing what
 * ships; `enforce` applies it.
 */
export type OverlayAclMode = 'off' | 'warn' | 'enforce'

export interface OverlayPolicy {
  id: string
  tenant_id: string
  name: string
  enabled: boolean
  sources: OverlaySelector[]
  via: OverlayTarget[]
  destinations: OverlayRule[]
  created_at: string
  updated_at: string
}

export interface OverlayPolicyCreate {
  name: string
  enabled: boolean
  sources: OverlaySelector[]
  via: OverlayTarget[]
  destinations: OverlayRule[]
}

interface OverlayPolicyListResponse {
  items: OverlayPolicy[]
  total: number
  page: number
  per_page: number
  mode: OverlayAclMode
}

export const useOverlayAclStore = defineStore('overlayAcl', () => {
  const policies = ref<OverlayPolicy[]>([])
  const total = ref(0)
  const mode = ref<OverlayAclMode>('off')
  const loading = ref(false)
  const error = ref<string | null>(null)
  /** The list stopped at the page cap (`MAX_PAGES` in utils/allPages.ts)
   *  while the server holds more rules: the tab says how many it shows. */
  const truncated = ref(false)
  /** #4248 — which org `policies` and `mode` describe: the org of the latest
   *  fetch, answered or not. A fetch for another org drops the rules and the
   *  posture first, so one org's never show under another org's page, and a
   *  save, a delete or a posture change answered for another org leaves this
   *  org's as they are. */
  const policiesTenantId = ref<string | null>(null)
  // Only the latest fetch writes: the answer for an org asked before it (the
  // page moved on to another org) is dropped.
  let seq = 0
  /** A save, a delete or a posture change answered for `tenantId` touches the
   *  list and the posture only when they are not another org's. */
  const listIsOf = (tenantId: string) =>
    policiesTenantId.value === null || policiesTenantId.value === tenantId

  // Fetches swallow into `error`; mutations THROW for the component to catch —
  // the split documented in stores/role.ts.
  async function fetchPolicies(tenantId: string) {
    const mySeq = ++seq
    if (policiesTenantId.value !== null && policiesTenantId.value !== tenantId) {
      policies.value = []
      total.value = 0
      mode.value = 'off'
    }
    policiesTenantId.value = tenantId
    loading.value = true
    error.value = null
    truncated.value = false
    try {
      // Every page, not the route's first 25 (#4188). The posture rides every
      // answer; like the total, the last one's is kept. A failed page lands in
      // the catch below like the one request did, and the posture stays.
      const all = await fetchAllPages<OverlayPolicy, OverlayPolicyListResponse>(
        `/tenant/${tenantId}/overlay-acl`,
      )
      if (mySeq !== seq) return
      policies.value = all.items
      total.value = all.last.total
      mode.value = all.last.mode
      truncated.value = all.truncated
    } catch (e) {
      if (mySeq !== seq) return
      error.value = (e as Error).message
      policies.value = []
      total.value = 0
    } finally {
      if (mySeq === seq) loading.value = false
    }
  }

  async function createPolicy(tenantId: string, payload: OverlayPolicyCreate) {
    const created = await api.post<OverlayPolicy>(
      `/tenant/${tenantId}/overlay-acl`,
      payload,
    )
    if (listIsOf(tenantId)) {
      policies.value = [created, ...policies.value]
      total.value += 1
    }
    return created
  }

  async function updatePolicy(
    tenantId: string,
    policyId: string,
    payload: OverlayPolicyCreate,
  ) {
    const updated = await api.put<OverlayPolicy>(
      `/tenant/${tenantId}/overlay-acl/${policyId}`,
      payload,
    )
    const i = policies.value.findIndex((p) => p.id === policyId)
    if (listIsOf(tenantId) && i !== -1) policies.value.splice(i, 1, updated)
    return updated
  }

  async function deletePolicy(tenantId: string, policyId: string) {
    await api.delete(`/tenant/${tenantId}/overlay-acl/${policyId}`)
    if (!listIsOf(tenantId)) return
    policies.value = policies.value.filter((p) => p.id !== policyId)
    total.value = Math.max(0, total.value - 1)
  }

  async function setMode(tenantId: string, next: OverlayAclMode) {
    const res = await api.put<{ mode: OverlayAclMode }>(
      `/tenant/${tenantId}/overlay-acl/mode`,
      { mode: next },
    )
    if (listIsOf(tenantId)) mode.value = res.mode
    return res.mode
  }

  return {
    policies,
    total,
    mode,
    loading,
    error,
    truncated,
    policiesTenantId,
    fetchPolicies,
    createPolicy,
    updatePolicy,
    deletePolicy,
    setMode,
  }
})
