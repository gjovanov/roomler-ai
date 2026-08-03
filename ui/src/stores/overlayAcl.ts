import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

// Matches `crates/remote_control/src/models.rs` (OverlaySelector / OverlayTarget
// / OverlayRule / OverlayAclMode) and the responses in
// `crates/api/src/routes/overlay_policy.rs`. snake_case mirrors the Rust wire
// shape, adjacently-tagged unions as `{kind, id}` like the tunnel-policy store.

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

  // Fetches swallow into `error`; mutations THROW for the component to catch —
  // the split documented in stores/role.ts.
  async function fetchPolicies(tenantId: string) {
    loading.value = true
    error.value = null
    try {
      const res = await api.get<OverlayPolicyListResponse>(
        `/tenant/${tenantId}/overlay-acl`,
      )
      policies.value = res.items
      total.value = res.total
      mode.value = res.mode
    } catch (e) {
      error.value = (e as Error).message
      policies.value = []
      total.value = 0
    } finally {
      loading.value = false
    }
  }

  async function createPolicy(tenantId: string, payload: OverlayPolicyCreate) {
    const created = await api.post<OverlayPolicy>(
      `/tenant/${tenantId}/overlay-acl`,
      payload,
    )
    policies.value = [created, ...policies.value]
    total.value += 1
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
    if (i !== -1) policies.value.splice(i, 1, updated)
    return updated
  }

  async function deletePolicy(tenantId: string, policyId: string) {
    await api.delete(`/tenant/${tenantId}/overlay-acl/${policyId}`)
    policies.value = policies.value.filter((p) => p.id !== policyId)
    total.value = Math.max(0, total.value - 1)
  }

  async function setMode(tenantId: string, next: OverlayAclMode) {
    const res = await api.put<{ mode: OverlayAclMode }>(
      `/tenant/${tenantId}/overlay-acl/mode`,
      { mode: next },
    )
    mode.value = res.mode
    return res.mode
  }

  return {
    policies,
    total,
    mode,
    loading,
    error,
    fetchPolicies,
    createPolicy,
    updatePolicy,
    deletePolicy,
    setMode,
  }
})
