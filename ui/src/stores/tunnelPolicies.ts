import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

// Snake-case + adjacently-tagged enums match the Rust wire shape —
// see `crates/api/src/routes/tunnel.rs::TunnelPolicyResponse` and
// `crates/remote_control/src/models.rs`.

export interface ExactHost {
  kind: 'exact'
  value: string
}
export interface WildcardHost {
  kind: 'wildcard'
  value: string
}
export interface CidrHost {
  kind: 'cidr'
  value: string
}
export type HostPattern = ExactHost | WildcardHost | CidrHost

export interface PortRange {
  low: number
  high: number
}

// `ProtocolKind` in `models.rs` — which L4 protocol a rule permits.
// `#[serde(default)]` on the Rust side means omitting it (or 'any')
// matches both TCP CONNECT and UDP ASSOCIATE forwards.
export type ProtocolKind = 'tcp' | 'udp' | 'any'

export interface DestinationRule {
  host_pattern: HostPattern
  port_range: PortRange
  proto?: ProtocolKind
}

// `PolicySubject` is `#[serde(tag = "kind", rename_all = "snake_case")]`
// in `models.rs`. The `id` field is renamed via `#[serde(rename = "id")]`
// on the inner field. Wire form is `{kind: "user_id", id: "<hex>"}`.
export interface UserIdSubject {
  kind: 'user_id'
  id: string
}
export interface RoleIdSubject {
  kind: 'role_id'
  id: string
}
export interface TunnelClientIdSubject {
  kind: 'tunnel_client_id'
  id: string
}
export interface AllUsersSubject {
  kind: 'all_users'
}
export type PolicySubject =
  | UserIdSubject
  | RoleIdSubject
  | TunnelClientIdSubject
  | AllUsersSubject

export interface AgentIdTarget {
  kind: 'agent_id'
  id: string
}
export interface AllAgentsTarget {
  kind: 'all_agents'
}
export type PolicyTarget = AgentIdTarget | AllAgentsTarget

export interface TunnelPolicy {
  id: string
  tenant_id: string
  name: string
  subjects: PolicySubject[]
  targets: PolicyTarget[]
  allowlist: DestinationRule[]
  max_concurrent_flows: number | null
  max_bytes_per_session: number | null
  created_at: string
  updated_at: string
}

export interface TunnelPolicyCreate {
  name: string
  subjects: PolicySubject[]
  targets: PolicyTarget[]
  allowlist: DestinationRule[]
  max_concurrent_flows?: number | null
  max_bytes_per_session?: number | null
}

// For update: `null` clears a ceiling, omit the field to leave it
// alone. Matches the Rust handler's two-level Option deserialiser.
export interface TunnelPolicyUpdate {
  name?: string
  subjects?: PolicySubject[]
  targets?: PolicyTarget[]
  allowlist?: DestinationRule[]
  max_concurrent_flows?: number | null
  max_bytes_per_session?: number | null
}

interface TunnelPolicyListResponse {
  items: TunnelPolicy[]
  total: number
  page: number
  per_page: number
  total_pages: number
}

export const useTunnelPolicyStore = defineStore('tunnelPolicies', () => {
  const policies = ref<TunnelPolicy[]>([])
  const total = ref(0)
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function fetchPolicies(tenantId: string) {
    loading.value = true
    error.value = null
    try {
      const resp = await api.get<TunnelPolicyListResponse>(
        `/tenant/${tenantId}/tunnel-policy`,
      )
      policies.value = resp.items
      total.value = resp.total
    } catch (e) {
      error.value = (e as Error).message
      policies.value = []
      total.value = 0
    } finally {
      loading.value = false
    }
  }

  async function createPolicy(
    tenantId: string,
    body: TunnelPolicyCreate,
  ): Promise<TunnelPolicy> {
    const created = await api.post<TunnelPolicy>(
      `/tenant/${tenantId}/tunnel-policy`,
      body,
    )
    policies.value = [created, ...policies.value]
    total.value += 1
    return created
  }

  async function updatePolicy(
    tenantId: string,
    policyId: string,
    body: TunnelPolicyUpdate,
  ): Promise<TunnelPolicy> {
    const updated = await api.put<TunnelPolicy>(
      `/tenant/${tenantId}/tunnel-policy/${policyId}`,
      body,
    )
    const idx = policies.value.findIndex((p) => p.id === policyId)
    if (idx >= 0) policies.value[idx] = updated
    return updated
  }

  async function deletePolicy(tenantId: string, policyId: string): Promise<void> {
    await api.delete(`/tenant/${tenantId}/tunnel-policy/${policyId}`)
    policies.value = policies.value.filter((p) => p.id !== policyId)
    total.value = Math.max(0, total.value - 1)
  }

  return {
    policies,
    total,
    loading,
    error,
    fetchPolicies,
    createPolicy,
    updatePolicy,
    deletePolicy,
  }
})
