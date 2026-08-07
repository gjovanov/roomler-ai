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
  async function fetchAdminMachines(tenantId: string, range: string): Promise<SeriesPayload> {
    return api.get<SeriesPayload>(`/admin/stats/machines?tenant_id=${tenantId}&range=${range}`)
  }
  async function fetchAdminCalls(range: string, tenantId?: string): Promise<SeriesPayload> {
    const t = tenantId ? `tenant_id=${tenantId}&` : ''
    return api.get<SeriesPayload>(`/admin/stats/calls?${t}range=${range}`)
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
    fetchAdminMachines,
    fetchAdminCalls,
  }
})
