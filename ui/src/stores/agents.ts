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

/** Codec + HW backend availability advertised by the agent in its
 *  rc:agent.hello payload. AgentsSection renders these as chips so
 *  operators can spot which agents support H.265 / AV1 etc. without
 *  starting a session. Phase 2 codec negotiation uses the union with
 *  the controller browser's capabilities to pick the best codec.
 *  Defaults to empty arrays for agents that haven't reconnected since
 *  the 2A.1 schema landed (server back-fills `Default::default()`). */
export interface AgentCapabilities {
  /** mime-style codec names: 'h264', 'h265', 'av1'. */
  codecs: string[]
  /** Descriptive backend labels: 'openh264-sw', 'mf-h264-hw', 'mf-h265-hw'. */
  hw_encoders: string[]
  has_input_permission: boolean
  supports_clipboard: boolean
  supports_file_transfer: boolean
  max_simultaneous_sessions: number
  /** File-DC v2 (0.3.0+) per-feature capability list. Recognised
   *  values: 'upload', 'download', 'download-folder', 'browse'.
   *  Empty / unset on older agents — browsers fall back to
   *  `supports_file_transfer` as the upload-only marker. */
  files?: string[]
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
  /** Optional because pre-2A.1 agents (and tests) may not include it. */
  capabilities?: AgentCapabilities
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

/** One agent-side crash report. Wire shape comes from
 *  `crates/remote_control/src/models.rs::AgentCrashPayload` (camelCase)
 *  plus server-attributed `id` + `reportedAt`. Reason values are the
 *  snake_case Rust enum discriminants (`panic` / `watchdog_stall` /
 *  `supervisor_detected`) — the chip-colour map in
 *  AgentCrashesDialog.vue keys off these EXACT strings. */
export interface AgentCrash {
  id: string
  reportedAt: string
  crashedAtUnix: number
  reason: 'panic' | 'watchdog_stall' | 'supervisor_detected'
  summary: string
  logTail: string
  agentVersion: string
  os: string
  hostname: string
  pid: number
}

interface AgentCrashListResponse {
  items: AgentCrash[]
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

  /** Fetch the most-recent 50 crash reports for an agent. No store
   *  caching — callers (AgentCrashesDialog) hold the result locally
   *  and refresh on demand via the modal's Refresh button. The
   *  endpoint is tenant-scoped on both sides; a foreign agentId
   *  returns an empty array, not an error. */
  async function fetchCrashes(
    tenantId: string,
    agentId: string,
  ): Promise<AgentCrash[]> {
    const resp = await api.get<AgentCrashListResponse>(
      `/tenant/${tenantId}/agent/${agentId}/crash`,
    )
    return resp.items
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
    fetchCrashes,
  }
})
