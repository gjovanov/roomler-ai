// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-116 — an overlay node, or an audit row's device, by the name a person
// knows the device by: joined on the wire's FK (agent_id / tunnel_client_id),
// never on the node's lossy DNS label.
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({ api: { get: vi.fn(async () => ({ items: [], total: 0 })), post: vi.fn(), put: vi.fn() } }))

import { useAgentStore } from '@/stores/agents'
import { useTunnelClientStore } from '@/stores/tunnelClients'
import { useNodeLabel } from '@/composables/useNodeLabel'

type AgentRow = ReturnType<typeof useAgentStore>['agents'][number]
type ClientRow = ReturnType<typeof useTunnelClientStore>['clients'][number]

describe('useNodeLabel', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    useAgentStore().agents = [
      { id: 'a1', name: 'reception-pc-2', display_name: 'Reception PC' },
      { id: 'a2', name: 'build-01' },
    ] as AgentRow[]
    useTunnelClientStore().clients = [
      { id: 'c1', name: 'neo-laptop', display_name: 'My laptop' },
    ] as ClientRow[]
  })

  it("names a node by its device's display name, found by the FK", () => {
    const { nodeLabel } = useNodeLabel()
    expect(nodeLabel({ name: 'reception-pc-2', agent_id: 'a1' })).toBe('Reception PC')
    expect(nodeLabel({ name: 'neo-laptop', tunnel_client_id: 'c1' })).toBe('My laptop')
  })

  it("falls back to the device's machine name, then the node's own label", () => {
    const { nodeLabel } = useNodeLabel()
    expect(nodeLabel({ name: 'build-01-2', agent_id: 'a2' })).toBe('build-01')
    // A node whose device is gone, or beyond the first page of the store.
    expect(nodeLabel({ name: 'old-node', agent_id: 'gone' })).toBe('old-node')
    expect(nodeLabel(null)).toBe('')
  })

  it('never joins on the name', () => {
    const { nodeLabel } = useNodeLabel()
    // Same label as a1's machine name, but no FK: the label stands.
    expect(nodeLabel({ name: 'reception-pc-2' })).toBe('reception-pc-2')
  })

  it("names an agent or a client by id, with the row's own fallback", () => {
    const { agentLabel, clientLabel } = useNodeLabel()
    expect(agentLabel('a1', 'x')).toBe('Reception PC')
    expect(agentLabel('zz', '64cd34…')).toBe('64cd34…')
    expect(clientLabel('c1')).toBe('My laptop')
    expect(clientLabel(undefined, 'n/a')).toBe('n/a')
  })
})
