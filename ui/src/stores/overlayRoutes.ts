import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

// Matches `crates/api/src/routes/overlay_route.rs::OverlayNodeResponse`.
export interface OverlayNode {
  id: string
  name: string
  overlay_ip: string
  kind: 'agent' | 'tunnel_client'
  advertised_routes: string[]
  approved_routes: string[]
  online: boolean
  last_seen_at: string
}

interface OverlayNodeListResponse {
  items: OverlayNode[]
}

export const useOverlayRoutesStore = defineStore('overlayRoutes', () => {
  const nodes = ref<OverlayNode[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function fetchNodes(tenantId: string) {
    loading.value = true
    error.value = null
    try {
      const resp = await api.get<OverlayNodeListResponse>(
        `/tenant/${tenantId}/overlay-node`,
      )
      nodes.value = resp.items
    } catch (e) {
      error.value = (e as Error).message
      nodes.value = []
    } finally {
      loading.value = false
    }
  }

  async function setApprovedRoutes(
    tenantId: string,
    nodeId: string,
    approvedRoutes: string[],
  ): Promise<OverlayNode> {
    const updated = await api.put<OverlayNode>(
      `/tenant/${tenantId}/overlay-node/${nodeId}/approved-routes`,
      { approved_routes: approvedRoutes },
    )
    const idx = nodes.value.findIndex((n) => n.id === nodeId)
    if (idx >= 0) nodes.value[idx] = updated
    return updated
  }

  return { nodes, loading, error, fetchNodes, setApprovedRoutes }
})
