import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

export type AgentOs = 'linux' | 'macos' | 'windows'
export type AgentStatusValue = 'online' | 'offline' | 'unenrolled' | 'quarantined'

export interface AccessPolicy {
  require_consent: boolean
  allowed_role_ids: string[]
  allowed_user_ids: string[]
  auto_terminate_idle_minutes: number | null
}

export interface Agent {
  id: string
  tenant_id: string
  owner_user_id: string
  name: string
  machine_id: string
  os: AgentOs
  agent_version: string
  status: AgentStatusValue
  is_online: boolean
  last_seen_at: string
  access_policy: AccessPolicy
}

export interface EnrollmentToken {
  enrollment_token: string
  expires_in: number
  jti: string
}

interface AgentListResponse {
  items: Agent[]
  total: number
  page: number
  per_page: number
  total_pages: number
}

export const useAgentStore = defineStore('agents', () => {
  const agents = ref<Agent[]>([])
  const total = ref(0)
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function fetchAgents(tenantId: string) {
    loading.value = true
    error.value = null
    try {
      const resp = await api.get<AgentListResponse>(`/tenant/${tenantId}/agent`)
      agents.value = resp.items
      total.value = resp.total
    } catch (e) {
      error.value = (e as Error).message
      agents.value = []
      total.value = 0
    } finally {
      loading.value = false
    }
  }

  async function issueEnrollmentToken(tenantId: string): Promise<EnrollmentToken> {
    return api.post<EnrollmentToken>(`/tenant/${tenantId}/agent/enroll-token`)
  }

  async function rename(tenantId: string, agentId: string, name: string) {
    await api.put(`/tenant/${tenantId}/agent/${agentId}`, { name })
    const idx = agents.value.findIndex((a) => a.id === agentId)
    if (idx !== -1) agents.value[idx]!.name = name
  }

  async function updateAccessPolicy(
    tenantId: string,
    agentId: string,
    policy: AccessPolicy,
  ) {
    await api.put(`/tenant/${tenantId}/agent/${agentId}`, { access_policy: policy })
    const idx = agents.value.findIndex((a) => a.id === agentId)
    if (idx !== -1) agents.value[idx]!.access_policy = policy
  }

  async function deleteAgent(tenantId: string, agentId: string) {
    await api.delete(`/tenant/${tenantId}/agent/${agentId}`)
    agents.value = agents.value.filter((a) => a.id !== agentId)
    total.value = Math.max(0, total.value - 1)
  }

  return {
    agents,
    total,
    loading,
    error,
    fetchAgents,
    issueEnrollmentToken,
    rename,
    updateAccessPolicy,
    deleteAgent,
  }
})
