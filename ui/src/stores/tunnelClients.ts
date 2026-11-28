// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import { api } from '@/api/client'
import { useCapabilitiesStore } from '@/stores/capabilities'
import type { DevicePresence } from '@/utils/devicePresence'

// Snake-case to match the Rust wire shape — see
// `crates/api/src/routes/tunnel.rs::TunnelClientResponse`.
export type TunnelClientOs = 'linux' | 'macos' | 'windows'
export type TunnelClientStatus = 'online' | 'offline' | 'unenrolled' | 'quarantined'

export interface TunnelClient {
  id: string
  tenant_id: string
  owner_user_id: string
  name: string
  /** Admin-set friendly label; display-only. */
  display_name?: string
  /** Admin-set fleet labels. */
  tags?: string[]
  machine_id: string
  os: TunnelClientOs
  client_version: string
  /** The admin's state (quarantined, …). Not presence: nothing the socket
   *  does writes it. */
  status: TunnelClientStatus
  last_seen_at: string
  /** #4553 — derived by the server from the client's live socket, on any of
   *  its pods. Absent from a server older than that. */
  presence?: DevicePresence
  /** `presence === 'online'`. */
  is_online?: boolean
}

export interface TunnelEnrollmentToken {
  enrollment_token: string
  expires_in: number
  jti: string
}

// Matches the `delete_tunnel_client` handler's JSON.
export interface DeletedTunnelClient {
  deleted: boolean
  overlay_released: boolean
  // `null` when the client never joined the overlay.
  overlay_ip: string | null
}

interface TunnelClientListResponse {
  items: TunnelClient[]
  total: number
  page: number
  per_page: number
  total_pages: number
}

export const useTunnelClientStore = defineStore('tunnelClients', () => {
  const clients = ref<TunnelClient[]>([])
  const total = ref(0)
  const loading = ref(false)
  const error = ref<string | null>(null)
  /** #4248 — which org `clients` describes: the org of the latest fetch,
   *  answered or not. A fetch for another org drops the rows first, so one
   *  org's clients never show (or fill a picker) under another org's page. */
  const clientsTenantId = ref<string | null>(null)
  // Only the latest fetch writes: the answer for an org asked before it (the
  // page moved on to another org) is dropped.
  let seq = 0
  // The latest fetch failed: `ensureTunnelClients` asks again.
  let failed = false
  /** #4553 — the clients connected now, by the presence the server derives
   *  from their sockets. Never by `status`: no socket writes it, so a count
   *  of it read 0 with every client connected. */
  const onlineCount = computed(() => clients.value.filter((c) => c.is_online === true).length)

  async function fetchTunnelClients(tenantId: string) {
    const mySeq = ++seq
    if (clientsTenantId.value !== null && clientsTenantId.value !== tenantId) {
      clients.value = []
      total.value = 0
    }
    clientsTenantId.value = tenantId
    failed = false
    // ⚠️ The gate belongs HERE, not at each call site. `/tenant/…/tunnel-client`
    // is a `network`-module route, and a server that drops the module answers
    // 404 — correctly. Gating it per-caller was tried and leaked: the org
    // dashboard guarded this fetch on `showFleet`, and `fleet` is a DIFFERENT
    // module, so a `remote` profile (fleet yes, network no) fired it anyway.
    // One guard in the store makes that class unrepresentable.
    // `has()` is FAIL-OPEN while the server has not answered, so this is inert
    // before first paint and in unit tests. (FR-75, #1447.)
    if (!useCapabilitiesStore().has('network')) {
      clients.value = []
      total.value = 0
      loading.value = false
      return
    }
    loading.value = true
    error.value = null
    try {
      // per_page=100 (the server cap): the parameterless request defaulted
      // to 25 server-side, so an online-count over `clients` against the
      // server `total` was the silent-truncation bug class.
      const resp = await api.get<TunnelClientListResponse>(
        `/tenant/${tenantId}/tunnel-client?per_page=100`,
      )
      if (mySeq !== seq) return
      clients.value = resp.items
      total.value = resp.total
    } catch (e) {
      if (mySeq !== seq) return
      failed = true
      error.value = (e as Error).message
      clients.value = []
      total.value = 0
    } finally {
      if (mySeq === seq) loading.value = false
    }
  }

  /** #4248 — the org's tunnel clients for a page that only names or picks
   *  them, fetched only when the store does not hold them already (the Devices
   *  page and the org dashboard load them). A store that holds another org's
   *  clients, or whose last fetch failed, fetches; one that holds this org's
   *  (or is loading them) does not. It replaces a check of an empty list,
   *  which never reloaded a list holding another org's clients. */
  function ensureTunnelClients(tenantId: string): Promise<void> {
    if (clientsTenantId.value === tenantId && !failed) return Promise.resolve()
    return fetchTunnelClients(tenantId)
  }

  async function issueEnrollmentToken(
    tenantId: string,
  ): Promise<TunnelEnrollmentToken> {
    return api.post<TunnelEnrollmentToken>(
      `/tenant/${tenantId}/tunnel-client/enroll-token`,
    )
  }

  /** Name / display_name / tags in one PUT — the ONLY in-place tunnel-client
   *  rename there is (a client-side rename derives a new machine_id and
   *  enrolls a brand-new row). Reads the additive envelope
   *  `{updated, client, dns_renamed, dns_name}`. */
  async function updateClient(
    tenantId: string,
    clientId: string,
    fields: { name?: string; display_name?: string; tags?: string[] },
  ): Promise<{ dnsRenamed?: boolean; dnsName?: string }> {
    const resp = await api.put<{
      updated?: boolean
      client?: TunnelClient
      dns_renamed?: boolean | null
      dns_name?: string | null
    }>(`/tenant/${tenantId}/tunnel-client/${clientId}`, fields)
    const idx = clients.value.findIndex((c) => c.id === clientId)
    if (idx !== -1) {
      if (resp?.client) {
        clients.value[idx] = { ...clients.value[idx]!, ...resp.client }
      } else {
        if (fields.name !== undefined) clients.value[idx]!.name = fields.name
        if (fields.display_name !== undefined)
          clients.value[idx]!.display_name = fields.display_name || undefined
        if (fields.tags !== undefined) clients.value[idx]!.tags = fields.tags
      }
    }
    return {
      dnsRenamed: resp?.dns_renamed ?? undefined,
      dnsName: resp?.dns_name ?? undefined,
    }
  }

  // Remove a tunnel client from the fleet. The server evicts its overlay node
  // first — peers get a `removes` delta and its overlay address goes back to the
  // tenant's pool, so it may later be assigned to a different machine.
  async function deleteTunnelClient(
    tenantId: string,
    clientId: string,
  ): Promise<DeletedTunnelClient> {
    const res = await api.delete<DeletedTunnelClient>(
      `/tenant/${tenantId}/tunnel-client/${clientId}`,
    )
    clients.value = clients.value.filter((c) => c.id !== clientId)
    total.value = Math.max(0, total.value - 1)
    return res
  }

  return {
    clients,
    total,
    onlineCount,
    loading,
    error,
    clientsTenantId,
    fetchTunnelClients,
    ensureTunnelClients,
    issueEnrollmentToken,
    updateClient,
    deleteTunnelClient,
  }
})
