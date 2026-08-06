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
  /** P6 — multi-user input arbitration: `free` (default — everyone with
   *  INPUT injects, agent-fenced) | `exclusive` (one floor holder,
   *  request/grant). `null`/absent = free. */
  input_mode?: 'free' | 'exclusive' | null
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
  /** Descriptive backend labels: 'openh264-sw', 'mf-h264-hw', 'mf-h265-hw',
   *  'ffmpeg-hevc_nvenc', 'ffmpeg-vp9_qsv', 'ffmpeg-av1_nvenc',
   *  'libvpx-vp9-444-sw'. The rc.190 HW×HW transport auto-rank reads
   *  these to know which codecs the agent HARDWARE-encodes. */
  hw_encoders: string[]
  /** DC video transports beyond the default WebRTC track:
   *  'data-channel-vp9-444', 'data-channel-hevc', 'data-channel-av1'.
   *  Serialized only when non-empty (serde skip_serializing_if), so
   *  it's optional here; the auto-rank falls back to deriving from
   *  `hw_encoders` for older agent rows. */
  transports?: string[]
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
  /** Audio codecs the agent can stream on the opt-in WebRTC audio
   *  track (system / desktop audio). Known value: `'opus'`. Empty /
   *  unset on older agents or agents built without the `audio` Cargo
   *  feature — the browser hides/disables the "receive audio" toggle
   *  when this doesn't contain `'opus'`. Mirrors `AgentCaps.audio` in
   *  `crates/remote_control/src/models.rs`. */
  audio?: string[]
  /** Fleet RPC. Known values: `'exec'` (honours rc:rpc.exec) and
   *  `'originate'` (its LocalAPI can drive `roomler exec` at other
   *  devices). Empty / unset on agents that predate the feature — the
   *  console tells the operator to update the agent rather than letting
   *  them send a frame it would silently drop. */
  rpc?: string[]
  /** rc.NEXT — remote app selection & launch on virtual-desktop hosts.
   *  Known values: 'list', 'focus', 'launch'. Empty / unset on older
   *  agents or non-VD hosts — the browser hides the Apps menu. Mirrors
   *  `AgentCaps.apps` in `crates/remote_control/src/models.rs`. */
  apps?: string[]
  /** Clipboard-DC protocol v2. Known values: 'ack' (write-ack replies
   *  gate the deferred Ctrl+V), 'events' (agent pushes host clipboard
   *  changes after `clipboard:subscribe`), 'images' (PNG payloads both
   *  directions), 'html' (v2.1 — CF_HTML + text alt round-trip:
   *  formatted text, tables, web-hosted images survive the paste),
   *  'native' (v2.2 — RTF with EMBEDDED images; needs the viewer's
   *  own local agent bridge to reach its RTF clipboard). Empty /
   *  unset on older agents — the browser falls back to the v1
   *  button-driven text-only flow. Mirrors `AgentCaps.clipboard` in
   *  `crates/remote_control/src/models.rs`. */
  clipboard?: string[]
  /** rc.227 — keyboard-layout integration (Windows hosts). Known
   *  values: 'report' (agent pushes rc:layout snapshots over the
   *  control DC), 'set' (agent accepts rc:layout.set manual switches).
   *  Empty / unset on older agents / non-Windows hosts — the browser
   *  hides the layout chip + picker. Mirrors `AgentCaps.layout`. */
  layout?: string[]
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
  /** Phase A-1 three-state truth: `online` = an rc socket is registered
   *  somewhere (Connect will work); `stale` = heartbeat trail fresh but no
   *  pod claims the socket (amber — half-open leg or dead pod); `offline`.
   *  Optional for pre-A-1 API bodies — consumers fall back to `is_online`. */
  presence?: 'online' | 'stale' | 'offline'
  /** Back-compat: `presence === 'online'`. */
  is_online: boolean
  last_seen_at: string
  access_policy: AccessPolicy
  /** Subnet-router CIDRs this agent advertises for the mesh subnet-router
   *  (Phase 2). Managed via the Subnet-routes dialog; the `roomler-tunnel`
   *  mesh longest-prefix-matches a LAN target IP against these to pick the
   *  covering agent. Optional because pre-Phase-2 agents / older API
   *  responses may omit it. */
  routes?: string[]
  /** Subnet CIDRs the agent itself ADVERTISES it can route (from its
   *  `advertise_routes` config, sent on hello). Untrusted suggestions the
   *  admin approves into `routes` via the Subnet-routes dialog. Optional /
   *  empty for pre-feature agents. */
  advertised_routes?: string[]
  /** Optional because pre-2A.1 agents (and tests) may not include it. */
  capabilities?: AgentCapabilities
  /** Fleet RPC gate 3. Absent on API bodies that predate the feature —
   *  consumers must treat that as `mode: 'off'`, never as permissive. */
  exec_policy?: ExecPolicy
  /** Multi-region relay PoPs: the agent's nearest relay region id (derived
   *  server-side from its STUN probe reports), e.g. "us-east". Absent/null =
   *  never probed or all probes timed out — the default region serves it. */
  relay_home?: string | null
}

/** Fleet RPC — whether a device accepts remote commands at all. Mirrors the
 *  Rust `ExecMode`. Default `off` on every device, including every device
 *  that existed before the feature. */
export type ExecMode = 'off' | 'on'

/** Fleet RPC gate 3 — the per-device execution policy.
 *
 *  Deliberately separate from {@link AccessPolicy}: that grants screen-view,
 *  and "may watch your screen" must never be the same checkbox as "may run a
 *  root shell". Commands inherit the daemon's identity — SYSTEM on a
 *  perMachine Windows install, root under systemd — so turning `mode` on is
 *  granting root on that device, which the dialog says in those words. */
export interface ExecPolicy {
  mode: ExecMode
  /** May this device ORIGINATE commands against others (`roomler exec` from
   *  its CLI)? Default false — without it, one compromised laptop would
   *  inherit its owner's exec rights across the whole fleet. */
  can_originate: boolean
  allowed_user_ids: string[]
  allowed_role_ids: string[]
  /** Only `auto` (unattended) and `prompt` are honoured; the session-shaped
   *  email/push modes collapse to `prompt`. `null` = prompt. */
  consent_mode: ConsentMode | null
  /** Empty = any shell the host supports. */
  shells: string[]
}

/** One remote command's result. `exit_code: null` together with a non-null
 *  `error` is how "never ran" (a gate refused, device offline, timed out)
 *  is distinguished from "ran and exited 0". */
export interface ExecResult {
  request_id: string
  agent_id: string
  agent_name: string
  exit_code: number | null
  stdout: string
  stderr: string
  truncated: boolean
  duration_ms: number
  error: string | null
}

/** Why an attempt was refused; `null` on the audit row means it ran. */
export type ExecDenyReason =
  | 'org_disabled'
  | 'no_permission'
  | 'device_disabled'
  | 'caller_not_allowed'
  | 'shell_not_allowed'
  | 'origin_not_allowed'
  | 'unsupported'
  | 'offline'
  | 'consent_denied'
  | 'rate_limited'
  | 'agent_disabled'

/** One row of the Fleet-RPC attempt log. Every attempt lands here, allowed
 *  or denied — a refused exec is the interesting one. */
export interface ExecAuditEntry {
  id?: string
  tenant_id: string
  agent_id: string
  user_id: string
  origin_agent_id?: string | null
  request_id: string
  source: string
  shell: string
  command: string
  at: string
  exit_code?: number | null
  duration_ms?: number | null
  denied?: ExecDenyReason | null
  output_sample?: string
  output_sha256?: string
  output_bytes?: number
  truncated?: boolean
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

  /** P4 — patch presence in place from a `device:presence` WS event for the
   *  ACTIVE org. `status` is left alone (it's the Mongo lifecycle field);
   *  `presence`/`is_online` are the reachability truth the list renders.
   *  Unknown agent ids are ignored — the next fetch converges. */
  function applyPresence(updates: Array<{ agent_id: string; presence: 'online' | 'stale' | 'offline' }>) {
    for (const u of updates) {
      const a = agents.value.find((x) => x.id === u.agent_id)
      if (!a) continue
      a.presence = u.presence
      a.is_online = u.presence === 'online'
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

  /** S1a — push an immediate self-update to one agent (`rc:agent.update`).
   *  Returns whether the message reached a live agent WS; offline agents
   *  pick the release up on their own periodic check. MANAGE_AGENTS. */
  async function triggerUpdate(tenantId: string, agentId: string): Promise<boolean> {
    const resp = await api.post<{ agent_id: string; delivered: boolean }>(
      `/tenant/${tenantId}/agent/${agentId}/update`,
      {},
    )
    return resp.delivered
  }

  /** S1a — push an immediate self-update to every agent in the tenant
   *  (or a selected subset via `agent_ids`). MANAGE_AGENTS. */
  async function triggerUpdateAll(
    tenantId: string,
  ): Promise<{ requested: number; delivered: number }> {
    return api.post<{ requested: number; delivered: number }>(
      `/tenant/${tenantId}/agent/update`,
      {},
    )
  }

  /** Multi-org — the organizations this device could be added to: every org
   *  where the caller holds MANAGE_AGENTS, minus the current one. `supported`
   *  is false for agents predating `rc:agent.join_org`, `online` false when
   *  there is no socket to push down; the dialog explains rather than
   *  letting the click fail. */
  async function fetchJoinTargets(
    tenantId: string,
    agentId: string,
  ): Promise<{
    items: Array<{
      tenant_id: string
      name: string
      slug: string
      already_enrolled: boolean
    }>
    supported: boolean
    online: boolean
  }> {
    return api.get(`/tenant/${tenantId}/agent/${agentId}/join-targets`)
  }

  /** Multi-org — add this device to another organization. Requires
   *  MANAGE_AGENTS in BOTH; the server mints a short-lived enrollment token
   *  and pushes it down the device's live socket. */
  async function joinOrg(
    tenantId: string,
    agentId: string,
    targetTenantId: string,
    opts: { label?: string; overlayMode?: string } = {},
  ): Promise<{ label: string; delivered: boolean; already_enrolled?: boolean }> {
    return api.post(`/tenant/${tenantId}/agent/${agentId}/join-org`, {
      target_tenant_id: targetTenantId,
      ...(opts.label ? { label: opts.label } : {}),
      ...(opts.overlayMode ? { overlay_mode: opts.overlayMode } : {}),
    })
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

  // ── Fleet RPC ────────────────────────────────────────────────────

  /** Whether the ORG allows remote execution at all (gate 1). `null` until
   *  fetched. Every device refuses while this is false, whatever its own
   *  policy says — the console shows that as the reason rather than letting
   *  an admin hunt through per-device settings. */
  const orgExecEnabled = ref<boolean | null>(null)

  async function fetchOrgExecEnabled(tenantId: string) {
    try {
      const resp = await api.get<{ remote_exec_enabled: boolean }>(
        `/tenant/${tenantId}/exec-settings`,
      )
      orgExecEnabled.value = resp.remote_exec_enabled
    } catch {
      // A 403 here means "not an admin", not "disabled" — leave it unknown
      // rather than claiming the org is off.
      orgExecEnabled.value = null
    }
  }

  /** Flip gate 1. MANAGE_TENANT server-side. */
  async function setOrgExecEnabled(tenantId: string, enabled: boolean) {
    const resp = await api.put<{ remote_exec_enabled: boolean }>(
      `/tenant/${tenantId}/exec-settings`,
      { remote_exec_enabled: enabled },
    )
    orgExecEnabled.value = resp.remote_exec_enabled
  }

  /** Replace a device's exec policy (gate 3). MANAGE_AGENTS server-side. */
  async function updateExecPolicy(tenantId: string, agentId: string, policy: ExecPolicy) {
    await api.put(`/tenant/${tenantId}/agent/${agentId}/exec-policy`, policy)
    const idx = agents.value.findIndex((a) => a.id === agentId)
    if (idx !== -1) agents.value[idx]!.exec_policy = policy
  }

  /** Run a command on one device.
   *
   *  Resolves even when the command was REFUSED — the server answers 200 with
   *  `error` set, so the caller renders one shape and never has to guess
   *  whether a rejection was a policy decision or a network failure. Only a
   *  malformed request or a missing device throws. */
  async function execOnAgent(
    tenantId: string,
    agentId: string,
    body: { shell?: string; command: string; timeout_ms?: number },
  ): Promise<ExecResult> {
    return await api.post<ExecResult>(`/tenant/${tenantId}/agent/${agentId}/exec`, body)
  }

  /** Run one command across several devices. An empty `agentIds` means every
   *  device in the org whose policy allows it. */
  async function execOnFleet(
    tenantId: string,
    agentIds: string[],
    body: { shell?: string; command: string; timeout_ms?: number },
  ): Promise<ExecResult[]> {
    const resp = await api.post<{ results: ExecResult[] }>(`/tenant/${tenantId}/agent/exec`, {
      agent_ids: agentIds,
      ...body,
    })
    return resp.results
  }

  /** Kill an in-flight command. The device still answers, so the pending
   *  request resolves with an error rather than hanging. */
  async function cancelExec(tenantId: string, agentId: string, requestId: string) {
    await api.post(`/tenant/${tenantId}/agent/${agentId}/exec/${requestId}/cancel`, {})
  }

  /** The attempt log. `agentId` narrows to one device's console history;
   *  `userId` answers "what did this person run?" — where an incident review
   *  starts. VIEW_EXEC_AUDIT server-side. */
  async function fetchExecAudit(
    tenantId: string,
    opts: { agentId?: string; userId?: string; page?: number; perPage?: number } = {},
  ): Promise<{ items: ExecAuditEntry[]; total: number }> {
    const q = new URLSearchParams()
    if (opts.agentId) q.set('agent_id', opts.agentId)
    if (opts.userId) q.set('user_id', opts.userId)
    q.set('page', String(opts.page ?? 1))
    q.set('per_page', String(opts.perPage ?? 50))
    const resp = await api.get<{ items: ExecAuditEntry[]; total: number }>(
      `/tenant/${tenantId}/exec-audit?${q.toString()}`,
    )
    return { items: resp.items, total: resp.total }
  }

  return {
    agents,
    total,
    loading,
    error,
    fetchAgents,
    applyPresence,
    issueEnrollmentToken,
    rename,
    updateAccessPolicy,
    updateRoutes,
    updateOwner,
    tenantMembers,
    fetchTenantMembers,
    triggerUpdate,
    triggerUpdateAll,
    fetchJoinTargets,
    joinOrg,
    deleteAgent,
    fetchCrashes,
    fetchLogs,
    orgExecEnabled,
    fetchOrgExecEnabled,
    setOrgExecEnabled,
    updateExecPolicy,
    execOnAgent,
    execOnFleet,
    cancelExec,
    fetchExecAudit,
  }
})
