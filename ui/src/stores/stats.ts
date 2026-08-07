import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

// Wire shapes mirror crates/api/src/routes/stats.rs. Every series point
// carries `t` (unix SECONDS) plus plain numbers — the server never leaks
// BSON dates/ObjectIds into these payloads.
export interface SeriesPoint {
  t: number
  [k: string]: number | null | undefined
}

export interface TenantOverview {
  enabled: boolean
  machines?: { online: number; total: number }
  calls?: { active: number; minutes_today: number }
  spark_machines?: SeriesPoint[]
  spark_minutes?: SeriesPoint[]
}

export interface SeriesPayload {
  enabled: boolean
  range?: string
  series?: SeriesPoint[]
  /** machines endpoint: per-agent totals over the window */
  agents?: Array<{
    agent_id: string
    online_minutes?: number
    cpu_pct?: number | null
    peer_rtt_ms?: number | null
  }>
  /** calls endpoint */
  totals?: { calls?: number; minutes?: number; participant_minutes?: number }
  /** machines endpoint: per-agent presence intervals for the uptime strip */
  uptime?: Array<{
    agent_id: string
    name?: string
    intervals: Array<{ from: number; to: number; presence: string }>
  }>
}

export interface RelayCurrent {
  enabled: boolean
  regions_enabled?: boolean
  regions?: Array<{
    id: string
    enabled: boolean
    monitored: boolean
    /** stats endpoints behind the region (>1 = aggregated multi-worker) */
    workers?: number
    busy: boolean
  }>
  latest?: Array<SeriesPoint & { region?: string; healthy?: boolean }>
  agent_rtt?: Array<{ region: string; rtt_avg_ms: number; agents: number }>
}

export interface OrgsPayload {
  enabled: boolean
  tenants?: Array<{ id: string; name: string; slug?: string }>
  machines?: Array<{ tenant_id: string; total: number; online: number }>
  /** membership counts — the third activity signal for hiding test orgs */
  members?: Array<{ tenant_id: string; members: number }>
  calls?: Array<{ tenant_id: string; calls_30d: number; minutes_30d: number }>
}

export interface MeshPayload {
  enabled: boolean
  center?: { id: string; name: string }
  /** overlay nodes — edges are keyed by these ids */
  nodes?: Array<{
    id: string
    agent_id_hex?: string
    name?: string
    overlay_ip?: string
    relay_home?: string | null
    status?: string
  }>
  /** agent rows carry presence + version, joined by hex id */
  agents?: Array<{
    id: string
    name?: string
    last_presence?: string
    agent_version?: string
    relay_home?: string | null
    os?: string
  }>
  edges?: Array<{
    kind: string
    from: string
    to: string
    carrier: string
    rtt_ms?: number | null
    stalled?: boolean
    reports?: number
  }>
}

export interface UsersPayload {
  enabled: boolean
  range?: string
  /** false = no GeoIP database configured ⇒ countries read "unknown" */
  geoip?: boolean
  series?: SeriesPoint[]
  browsers?: Array<{ key: string; sessions: number }>
  platforms?: Array<{ key: string; sessions: number }>
  countries?: Array<{ key: string; sessions: number }>
  orgs?: Array<{
    tenant_id: string
    sessions: number
    users: number
    connected_minutes: number
  }>
  pages?: Array<{ path: string; views: number; users: number }>
  durations?: Array<{ bucket: string; sessions: number }>
}

// ── Wave 3 — per-user usage. Mirrors crates/api/src/routes/usage.rs. ──

/** One activity class's totals. `bytes_known: false` means the bytes were
 *  never measured — render an em dash, NOT a zero. */
export interface UsageClass {
  minutes: number
  bytes: number
  sessions: number
  devices: number
  bytes_known: boolean
}

export interface UsageUserRow {
  user_id: string
  name: string
  rc: UsageClass
  call: UsageClass
  tunnel: UsageClass
  total_minutes: number
  /** Platform scope only — which orgs this user was active in. */
  orgs?: Array<{ tenant_id: string; name: string }>
}

export interface UsagePayload {
  enabled: boolean
  range?: string
  /** False when the range outruns the 90-day remote_audit TTL, so the UI
   *  can say watcher history is incomplete rather than implying none. */
  watchers_complete?: boolean
  users?: UsageUserRow[]
}

export interface UsageViewingWindow {
  session_id: string
  agent_id: string
  agent_name?: string
  tenant_id: string
  tenant_name?: string
  started_at: number
  ended_at?: number | null
  seconds: number
  role: 'controller' | 'watcher'
  bytes?: number
  bytes_known?: boolean
}

export interface UsageDetailPayload {
  enabled: boolean
  range?: string
  watchers_complete?: boolean
  user?: { user_id: string; name: string }
  totals?: {
    rc_minutes: number
    rc_bytes: number
    call_minutes: number
    call_bytes: number
    tunnel_minutes: number
  }
  /** The headline: every window this user spent looking at a screen. */
  viewing?: UsageViewingWindow[]
  calls?: Array<{
    room_id: string
    room_name?: string
    tenant_id: string
    tenant_name?: string
    started_at: number
    ended_at?: number | null
    seconds: number
  }>
  tunnels?: Array<{
    session_id?: string
    agent_id?: string
    agent_name?: string
    tenant_id?: string
    tenant_name?: string
    started_at: number
    ended_at: number
    seconds: number
    events: number
    bytes_known: boolean
  }>
  truncated?: boolean
}

export const useStatsStore = defineStore('stats', () => {
  // Cached snapshots the dashboard panels poll into. Query-tab payloads
  // are returned to the caller (view-local state) — they're range-keyed
  // and short-lived, caching them here would just go stale.
  const overview = ref<TenantOverview | null>(null)
  const relayCurrent = ref<RelayCurrent | null>(null)
  const mesh = ref<MeshPayload | null>(null)
  const error = ref<string | null>(null)

  async function fetchOverview(tenantId: string): Promise<TenantOverview | null> {
    try {
      overview.value = await api.get<TenantOverview>(`/tenant/${tenantId}/stats/overview`)
      error.value = null
    } catch (e) {
      // 404 = not a member / stats hidden — treat as "no panel", never throw
      // into the dashboard.
      error.value = e instanceof Error ? e.message : 'overview failed'
      overview.value = null
    }
    return overview.value
  }

  async function fetchMachines(tenantId: string, range: string): Promise<SeriesPayload> {
    return api.get<SeriesPayload>(`/tenant/${tenantId}/stats/machines?range=${range}`)
  }
  async function fetchCalls(tenantId: string, range: string): Promise<SeriesPayload> {
    return api.get<SeriesPayload>(`/tenant/${tenantId}/stats/calls?range=${range}`)
  }
  async function fetchTunnels(tenantId: string, range: string): Promise<SeriesPayload> {
    return api.get<SeriesPayload>(`/tenant/${tenantId}/stats/tunnels?range=${range}`)
  }

  /** Overlay topology for the dashboard mesh graph (member-visible). */
  async function fetchMesh(tenantId: string): Promise<MeshPayload | null> {
    try {
      mesh.value = await api.get<MeshPayload>(`/tenant/${tenantId}/stats/mesh`)
      error.value = null
    } catch (e) {
      // 404 = not a member / stats off — the panel just doesn't render.
      error.value = e instanceof Error ? e.message : 'mesh failed'
      mesh.value = null
    }
    return mesh.value
  }

  // ── platform admin ────────────────────────────────────────────────────
  async function fetchRelayCurrent(): Promise<RelayCurrent | null> {
    try {
      relayCurrent.value = await api.get<RelayCurrent>('/admin/stats/relay/current')
      error.value = null
    } catch (e) {
      error.value = e instanceof Error ? e.message : 'relay current failed'
      relayCurrent.value = null
    }
    return relayCurrent.value
  }
  async function fetchRelayHistory(region: string, range: string): Promise<SeriesPayload> {
    return api.get<SeriesPayload>(
      `/admin/stats/relay/history?region=${encodeURIComponent(region)}&range=${range}`,
    )
  }
  async function fetchOrgs(): Promise<OrgsPayload> {
    return api.get<OrgsPayload>('/admin/stats/orgs')
  }
  async function fetchUsers(range: string): Promise<UsersPayload> {
    return api.get<UsersPayload>(`/admin/stats/users?range=${range}`)
  }
  async function fetchAdminMachines(tenantId: string, range: string): Promise<SeriesPayload> {
    return api.get<SeriesPayload>(`/admin/stats/machines?tenant_id=${tenantId}&range=${range}`)
  }
  async function fetchAdminCalls(range: string, tenantId?: string): Promise<SeriesPayload> {
    const t = tenantId ? `tenant_id=${tenantId}&` : ''
    return api.get<SeriesPayload>(`/admin/stats/calls?${t}range=${range}`)
  }

  // ── Wave 3 — per-user usage ──────────────────────────────────────────
  async function fetchTenantUsage(tenantId: string, range: string): Promise<UsagePayload> {
    return api.get<UsagePayload>(`/tenant/${tenantId}/stats/usage?range=${range}`)
  }
  async function fetchTenantUsageDetail(
    tenantId: string,
    userId: string,
    range: string,
  ): Promise<UsageDetailPayload> {
    return api.get<UsageDetailPayload>(
      `/tenant/${tenantId}/stats/usage/${userId}?range=${range}`,
    )
  }
  async function fetchAdminUsage(range: string, tenantId?: string): Promise<UsagePayload> {
    const t = tenantId ? `tenant_id=${tenantId}&` : ''
    return api.get<UsagePayload>(`/admin/stats/usage?${t}range=${range}`)
  }
  async function fetchAdminUsageDetail(
    userId: string,
    range: string,
    tenantId?: string,
  ): Promise<UsageDetailPayload> {
    const t = tenantId ? `tenant_id=${tenantId}&` : ''
    return api.get<UsageDetailPayload>(`/admin/stats/usage/${userId}?${t}range=${range}`)
  }

  return {
    overview,
    relayCurrent,
    mesh,
    error,
    fetchOverview,
    fetchMachines,
    fetchCalls,
    fetchTunnels,
    fetchMesh,
    fetchRelayCurrent,
    fetchRelayHistory,
    fetchOrgs,
    fetchUsers,
    fetchAdminMachines,
    fetchAdminCalls,
    fetchTenantUsage,
    fetchTenantUsageDetail,
    fetchAdminUsage,
    fetchAdminUsageDetail,
  }
})
