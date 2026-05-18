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

  return {
    clients,
    total,
    loading,
    error,
    fetchTunnelClients,
    issueEnrollmentToken,
  }
})
