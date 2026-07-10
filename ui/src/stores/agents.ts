import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

export type AgentOs = 'linux' | 'macos' | 'windows'
export type AgentStatusValue = 'online' | 'offline' | 'unenrolled' | 'quarantined'

/** How consent is obtained before a controller may drive a device. Mirrors the
 *  Rust `ConsentMode` (snake_case). `null` = inherit the system default
 *  (`prompt` — attended). Replaces the legacy `require_consent` bool. */
export type ConsentMode = 'auto' | 'prompt' | 'email' | 'push' | 'prompt_then_email'

export interface AccessPolicy {
  consent_mode: ConsentMode | null
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
  /** rc.61 — VP9 chroma format the agent emits on the
   *  `data-channel-vp9-444` transport. Values: `'yuv444'` (default,
   *  VP9 profile 1, sharpest text via ClearType chroma) or
   *  `'yuv420'` (VP9 profile 0, ~30% bandwidth saving with slight
   *  chroma softening on small Windows text). Empty / unset on
   *  pre-rc.61 agents — browsers treat as `'yuv444'`. The vp9-444
   *  worker uses this to pick the right `VideoDecoder` codec
   *  string (`vp09.01.10.08` vs `vp09.00.10.08`); mismatch leaves
   *  the canvas blank. */
  vp9_chroma?: string
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
  /** Subnet-router CIDRs this agent advertises for the mesh subnet-router
   *  (Phase 2). Managed via the Subnet-routes dialog; the `roomler-tunnel`
   *  mesh longest-prefix-matches a LAN target IP against these to pick the
   *  covering agent. Optional because pre-Phase-2 agents / older API
   *  responses may omit it. */
  routes?: string[]
  /** Optional because pre-2A.1 agents (and tests) may not include it. */
  capabilities?: AgentCapabilities
}

/** A tenant member as returned by `GET /tenant/{id}/member` — enough to populate
 *  the owner-reassign picker + resolve `owner_user_id` to a name. */
export interface TenantMember {
  user_id: string
  display_name: string
  nickname: string | null
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

/** One uploaded log line. Wire shape from
 *  `crates/db/src/models/agent_log.rs::LogLine` serialised through
 *  `crates/api/src/routes/agent_log.rs`. `level` is the UPPERCASE
 *  Rust enum discriminant (TRACE/DEBUG/INFO/WARN/ERROR); `fields`
 *  is an arbitrary structured-field object (may be empty). */
export interface AgentLogLine {
  ts: string
  level: 'TRACE' | 'DEBUG' | 'INFO' | 'WARN' | 'ERROR'
  target: string
  msg: string
  fields: Record<string, unknown>
}

/** One uploaded log batch. Mirrors `AgentLogBatchView` in
 *  `crates/api/src/routes/agent_log.rs`. `source` is the lowercase
 *  Rust enum (`agent`/`service`/`installer`/`crash`/`updater`/
 *  `browser`); `createdAt` is the server ingest timestamp (RFC3339). */
export interface AgentLogBatch {
  id: string
  source: string
  agentId: string | null
  userId: string | null
  sessionId: string | null
  hostIdHash: string | null
  agentVersion: string | null
  lineCount: number
  createdAt: string
  lines: AgentLogLine[]
}

interface AgentLogsListResponse {
  batches: AgentLogBatch[]
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

  /** Replace the agent's advertised subnet-router CIDRs (mesh Phase 2). A
   *  MANAGE_AGENTS admin action. The server validates + canonicalizes each
   *  CIDR (masks host bits, dedups) and rejects invalid input with 400; we
   *  optimistically patch local state with the caller's already-canonicalized
   *  list. */
  async function updateRoutes(tenantId: string, agentId: string, routes: string[]) {
    await api.put(`/tenant/${tenantId}/agent/${agentId}`, { routes })
    const idx = agents.value.findIndex((a) => a.id === agentId)
    if (idx !== -1) agents.value[idx]!.routes = routes
  }

  /** Reassign the device owner (a MANAGE_AGENTS admin action). The owner is who
   *  self-controls without an allowlist entry + who consent routes to. */
  async function updateOwner(tenantId: string, agentId: string, ownerUserId: string) {
    await api.put(`/tenant/${tenantId}/agent/${agentId}`, { owner_user_id: ownerUserId })
    const idx = agents.value.findIndex((a) => a.id === agentId)
    if (idx !== -1) agents.value[idx]!.owner_user_id = ownerUserId
  }

  /** Tenant members — for the owner-reassign picker + resolving an agent's
   *  `owner_user_id` to a display name. Fetched on demand by AgentsSection. */
  const tenantMembers = ref<TenantMember[]>([])
  async function fetchTenantMembers(tenantId: string) {
    try {
      const resp = await api.get<{ items: TenantMember[] }>(`/tenant/${tenantId}/member`)
      tenantMembers.value = resp.items
    } catch {
      tenantMembers.value = []
    }
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

  /** Fetch the most-recent uploaded log batches for an agent (rc.58/
   *  rc.59 centralized log backbone). `limit` is the number of
   *  BATCHES, not lines (the server clamps to 1..=500; default 50).
   *  No store caching — the AgentLogsDialog holds the result and
   *  refreshes on demand. Tenant-scoped on both sides; a foreign
   *  agentId yields an empty list, not an error. */
  async function fetchLogs(
    tenantId: string,
    agentId: string,
    limit = 50,
  ): Promise<AgentLogBatch[]> {
    const resp = await api.get<AgentLogsListResponse>(
      `/tenant/${tenantId}/agent/${agentId}/logs?limit=${limit}`,
    )
    return resp.batches
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
    updateRoutes,
    updateOwner,
    tenantMembers,
    fetchTenantMembers,
    deleteAgent,
    fetchCrashes,
    fetchLogs,
  }
})
