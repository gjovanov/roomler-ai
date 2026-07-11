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
    fetchMagicDns,
    saveMagicDns,
  }
})
