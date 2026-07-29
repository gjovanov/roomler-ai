import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

// Snake-case to match the Rust wire shape — see
// `crates/api/src/routes/tunnel.rs::TunnelClientResponse`.
export type TunnelClientOs = 'linux' | 'macos' | 'windows'
export type TunnelClientStatus = 'online' | 'offline' | 'unenrolled' | 'quarantined'

export interface TunnelClient {
  id: string
  tenant_id: string
  owner_user_id: string
  name: string
  machine_id: string
  os: TunnelClientOs
  client_version: string
  status: TunnelClientStatus
  last_seen_at: string
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

  async function fetchTunnelClients(tenantId: string) {
    loading.value = true
    error.value = null
    try {
      const resp = await api.get<TunnelClientListResponse>(
        `/tenant/${tenantId}/tunnel-client`,
      )
      clients.value = resp.items
      total.value = resp.total
    } catch (e) {
      error.value = (e as Error).message
      clients.value = []
      total.value = 0
    } finally {
      loading.value = false
    }
  }

  async function issueEnrollmentToken(
    tenantId: string,
  ): Promise<TunnelEnrollmentToken> {
    return api.post<TunnelEnrollmentToken>(
      `/tenant/${tenantId}/tunnel-client/enroll-token`,
    )
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
    loading,
    error,
    fetchTunnelClients,
    issueEnrollmentToken,
    deleteTunnelClient,
  }
})
