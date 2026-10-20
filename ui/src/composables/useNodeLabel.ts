// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { computed } from 'vue'
import { useAgentStore } from '@/stores/agents'
import { useTunnelClientStore } from '@/stores/tunnelClients'
import { deviceLabel } from '@/composables/useHideDeviceName'

/**
 * FR-116 — an overlay node, or anything that names a device by id, by the
 * name a person knows the device by.
 *
 * `OverlayNode.name` is a lossy, de-duplicated DNS label (`reception-pc-2`),
 * and audit rows carry the machine name the server stored. The Devices page
 * shows the admin-set display name, so a page that showed the label named the
 * device a person had just tapped differently. The node carries its device's
 * id on the wire (`agent_id` / `tunnel_client_id`), and that is the join —
 * never the name.
 *
 * ⚠️ For people only. A MagicDNS label, an `overlay_exit_node = "…"` line or a
 * CLI selector keeps the technical name.
 */
export function useNodeLabel() {
  const agentStore = useAgentStore()
  const tunnelClientStore = useTunnelClientStore()

  const agentsById = computed(() => new Map(agentStore.agents.map((a) => [a.id, a])))
  const clientsById = computed(() => new Map(tunnelClientStore.clients.map((c) => [c.id, c])))

  /** A node (or a node-shaped row) by its device's label; the node's own name
   *  only when its device is not known here (gone, or beyond the first page). */
  function nodeLabel(
    n: { name?: string | null; agent_id?: string | null; tunnel_client_id?: string | null } | null | undefined,
  ): string {
    if (!n) return ''
    const agent = n.agent_id ? agentsById.value.get(n.agent_id) : undefined
    const client = n.tunnel_client_id ? clientsById.value.get(n.tunnel_client_id) : undefined
    return deviceLabel(agent ?? client ?? n)
  }

  /** An agent by id, by its label — `fallback` (what the row itself carries)
   *  when the agent is not in the store. */
  function agentLabel(agentId: string | null | undefined, fallback = ''): string {
    const a = agentId ? agentsById.value.get(agentId) : undefined
    return a ? deviceLabel(a) : fallback
  }

  /** A tunnel client by id, by its label, else `fallback`. */
  function clientLabel(clientId: string | null | undefined, fallback = ''): string {
    const c = clientId ? clientsById.value.get(clientId) : undefined
    return c ? deviceLabel(c) : fallback
  }

  /** Load what the labels read, once, when nobody has yet. The shell loads the
   *  agents for its nav; tunnel clients only the Devices page does. */
  function ensureLoaded(tenantId: string, opts: { tunnels?: boolean } = {}) {
    if (agentStore.agents.length === 0) void agentStore.fetchAgents(tenantId)
    if (opts.tunnels && tunnelClientStore.clients.length === 0) {
      void tunnelClientStore.fetchTunnelClients(tenantId)
    }
  }

  return { nodeLabel, agentLabel, clientLabel, ensureLoaded }
}
