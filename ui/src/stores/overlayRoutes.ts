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
  // P5 — admin has designated this node as an exit node (routes 0.0.0.0/0).
  is_exit_node: boolean
  // P5 — node advertised 0.0.0.0/0, so it's eligible to be toggled on.
  can_be_exit_node: boolean
  online: boolean
  // The backing device is still enrolled, so evicting this node from the mesh
  // does NOT keep it out — it rejoins on its next connect with a NEW overlay
  // address. `false` means the device is gone and the eviction is permanent.
  will_rejoin: boolean
  last_seen_at: string
}

interface OverlayNodeListResponse {
  items: OverlayNode[]
}

// Matches `overlay_route.rs::EvictOverlayNodeResponse`.
export interface EvictedNode {
  released: boolean
  node_id: string
  name: string
  overlay_ip: string
  // `false` when the address could not be returned to the pool (it still left
  // the mesh — the address just isn't reused).
  host_recycled: boolean
}

// A node's *derived* overlay IPv6: its overlay v4 embedded in the low 32 bits
// of Roomler's ULA /96 (`fd72:6f6f:6d6c::<v4>`). Mirrors
// `crates/tunnel-core/src/overlay/router.rs::derive_overlay_v6` — display-only
// here (routing derives it node-side), so the server never has to publish v6.
// Matches Rust's `Ipv6Addr` Display: `::` swallows the zero run, hex segments
// carry no leading zeros, and a zero high segment folds into the `::`.
export function deriveOverlayV6(v4: string): string | null {
  const parts = v4.split('.').map(Number)
  if (parts.length !== 4 || parts.some((p) => !Number.isInteger(p) || p < 0 || p > 255)) {
    return null
  }
  const hi = (parts[0] << 8) | parts[1]
  const lo = (parts[2] << 8) | parts[3]
  if (hi === 0 && lo === 0) return 'fd72:6f6f:6d6c::'
  if (hi === 0) return `fd72:6f6f:6d6c::${lo.toString(16)}`
  return `fd72:6f6f:6d6c::${hi.toString(16)}:${lo.toString(16)}`
}

// Matches `overlay_route.rs::MagicDnsResponse` / `SetMagicDnsRequest`.
export interface MagicDnsSettings {
  magic_dns_domain: string | null
  magic_dns_nameservers: string[]
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

  // P5 — designate (or un-designate) a node as an exit node. The server adds/
  // removes 0.0.0.0/0 in approved_routes and re-fans peers.
  async function setExitNode(
    tenantId: string,
    nodeId: string,
    enabled: boolean,
  ): Promise<OverlayNode> {
    const updated = await api.put<OverlayNode>(
      `/tenant/${tenantId}/overlay-node/${nodeId}/exit-node`,
      { enabled },
    )
    const idx = nodes.value.findIndex((n) => n.id === nodeId)
    if (idx >= 0) nodes.value[idx] = updated
    return updated
  }

  // Evict a node from the overlay mesh. The server fans an
  // OverlayNetmapDelta{removes:[id]} to every peer BEFORE responding, and
  // returns the node's overlay address to the tenant's pool — so it may later
  // be assigned to a DIFFERENT machine. A node whose backing device is still
  // enrolled (`will_rejoin`) comes back on its next connect with a new address.
  async function evictNode(tenantId: string, nodeId: string): Promise<EvictedNode> {
    const res = await api.delete<EvictedNode>(
      `/tenant/${tenantId}/overlay-node/${nodeId}`,
    )
    nodes.value = nodes.value.filter((n) => n.id !== nodeId)
    return res
  }

  async function fetchMagicDns(tenantId: string): Promise<MagicDnsSettings> {
    return await api.get<MagicDnsSettings>(`/tenant/${tenantId}/magic-dns`)
  }

  async function saveMagicDns(
    tenantId: string,
    settings: MagicDnsSettings,
  ): Promise<MagicDnsSettings> {
    return await api.put<MagicDnsSettings>(
      `/tenant/${tenantId}/magic-dns`,
      settings,
    )
  }

  return {
    nodes,
    loading,
    error,
    fetchNodes,
    setApprovedRoutes,
    setExitNode,
    evictNode,
    fetchMagicDns,
    saveMagicDns,
  }
})
