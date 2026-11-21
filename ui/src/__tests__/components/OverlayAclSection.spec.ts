// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3636 — the Overlay ACL tab names every source and via node a rule names by
// id, and its editor opens and saves such a rule.
//
// The server answers each id as the 24-hex string `docs/api.md` promises (the
// fixture below). Before the fix it sent bson's `{"$oid": …}`: no lookup
// matched, so every chip fell back to a bare "Node", "User" or "Role", and the
// editor's pickers opened on no node, member or role. The editor is a
// teleported v-dialog, so the spec mounts attached to the document and reads
// the dialog from there: a stub that hid the dialog's body would pass on the
// broken shape too.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi, type MockInstance } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

const ids = vi.hoisted(() => ({
  node: '6a7e1c2d3b4a59687f0e1d40',
  device: '6a7e1c2d3b4a59687f0e1d41',
  user: '6a7e1c2d3b4a59687f0e1d42',
  role: '6a7e1c2d3b4a59687f0e1d43',
}))

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))
vi.mock('@/stores/overlayRoutes', () => ({
  useOverlayRoutesStore: () => ({
    nodes: [
      { id: ids.node, name: 'lab-01', overlay_ip: '100.64.0.7', kind: 'agent', agent_id: ids.device },
    ],
    fetchNodes: vi.fn(),
  }),
}))
vi.mock('@/stores/agents', () => ({
  useAgentStore: () => ({
    agents: [{ id: ids.device, name: 'lab-01', display_name: 'Lab box' }],
    fetchAgents: vi.fn(),
  }),
}))
vi.mock('@/stores/tunnelClients', () => ({
  useTunnelClientStore: () => ({ clients: [], fetchTunnelClients: vi.fn() }),
}))
vi.mock('@/stores/role', () => ({
  useRoleStore: () => ({
    roles: [{ id: ids.role, name: 'Mesh operators' }],
    fetchRoles: vi.fn(),
  }),
}))
vi.mock('@/stores/members', () => ({
  useMembersStore: () => ({
    items: [{ user_id: ids.user, display_name: 'Ops lead', nickname: 'ops-lead' }],
    fetchMembers: vi.fn(),
  }),
}))

import OverlayAclSection from '@/components/admin/OverlayAclSection.vue'
import type { OverlayPolicy } from '@/stores/overlayAcl'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const TENANT_ID = '69a1dbbad2000f26adc875ce'
const RULE_ID = '6a7e1c2d3b4a59687f0e1d50'
const CATCH_ALL_ID = '6a7e1c2d3b4a59687f0e1d51'
const vuetify = createVuetify({ components, directives })

/** An id as the server writes it on the wire: the 24-hex string (#3636). */
const wireId = (hex: string): string => hex

/** The node's picker and chip title: its device's name, then its address. */
const NODE = 'Lab box — 100.64.0.7'

function namedRule(): OverlayPolicy {
  return {
    id: RULE_ID,
    tenant_id: TENANT_ID,
    name: 'ops reach the lab',
    enabled: true,
    sources: [
      { kind: 'node_id', id: wireId(ids.node) },
      { kind: 'user_id', id: wireId(ids.user) },
      { kind: 'role_id', id: wireId(ids.role) },
    ],
    via: [{ kind: 'node_id', id: wireId(ids.node) }],
    destinations: [{ cidr: '192.0.2.1/32', port_range: { low: 9, high: 9 }, proto: 'tcp' }],
    created_at: '2026-11-20T00:00:00Z',
    updated_at: '2026-11-20T00:00:00Z',
  }
}

function catchAllRule(): OverlayPolicy {
  return {
    ...namedRule(),
    id: CATCH_ALL_ID,
    name: 'every node, everywhere',
    sources: [{ kind: 'all_nodes' }],
    via: [{ kind: 'all_nodes' }],
  }
}

function listOf(items: OverlayPolicy[]) {
  return { items, total: items.length, page: 1, per_page: 25, mode: 'warn' }
}

beforeAll(() => {
  // Vuetify overlays measure themselves against the viewport; jsdom has
  // neither a ResizeObserver nor a visualViewport, and without the latter the
  // editor never opens.
  const g = globalThis as unknown as Record<string, unknown>
  if (!('ResizeObserver' in g)) {
    g.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    }
  }
  if (!('visualViewport' in g)) {
    g.visualViewport = {
      width: 1280,
      height: 900,
      offsetLeft: 0,
      offsetTop: 0,
      pageLeft: 0,
      pageTop: 0,
      scale: 1,
      addEventListener() {},
      removeEventListener() {},
    }
  }
})

let pinia: Pinia
let raised: unknown[]
let consoleError: MockInstance
let consoleWarn: MockInstance

beforeEach(() => {
  pinia = createPinia()
  setActivePinia(pinia)
  vi.clearAllMocks()
  raised = []
  consoleError = vi.spyOn(console, 'error')
  consoleWarn = vi.spyOn(console, 'warn')
})

afterEach(() => {
  consoleError.mockRestore()
  consoleWarn.mockRestore()
  document.body.innerHTML = ''
})

function mountSection() {
  return mount(OverlayAclSection, {
    props: { tenantId: TENANT_ID },
    global: {
      plugins: [vuetify, pinia],
      config: { errorHandler: (err: unknown) => void raised.push(err) },
    },
    attachTo: document.body,
  })
}

/** Nothing raised while rendering or handling a click, nothing logged. */
function expectNothingRaised() {
  expect(raised.map(String)).toEqual([])
  expect(consoleError).not.toHaveBeenCalled()
  expect(consoleWarn).not.toHaveBeenCalled()
}

/** The chips of one cell of the rule table, as a person reads them. */
function chips(row: Element, column: 'Sources' | 'Via'): string[] {
  return [...row.querySelectorAll(`td[data-label="${column}"] .v-chip`)].map(
    (c) => c.textContent?.trim() ?? '',
  )
}

/** The open dialog: Vuetify teleports it out of the component's own tree. */
function dialog(): Element {
  const d = document.querySelector('.v-overlay--active .v-overlay__content')
  if (!d) throw new Error('the editor did not open')
  return d
}

/** What every picker of the open editor shows, in order; '' for an empty one. */
function pickers(): string[] {
  return [...dialog().querySelectorAll('.v-select')].map(
    (s) => s.querySelector('.v-select__selection-text')?.textContent?.trim() ?? '',
  )
}

async function openEditor(row: number) {
  const pencil = document.querySelectorAll('tbody tr')[row]?.querySelector('.mdi-pencil')
  const button = pencil?.closest('button')
  if (!button) throw new Error(`row ${row} has no edit button`)
  button.click()
  await flushPromises()
}

describe('OverlayAclSection — a rule that names its sources and via nodes by id (#3636)', () => {
  it('names each one: the node by its device, the member and the role by name', async () => {
    mockApi.get.mockResolvedValue(listOf([namedRule(), catchAllRule()]))
    const w = mountSection()
    await flushPromises()
    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/overlay-acl`)
    const rows = [...document.querySelectorAll('tbody tr')]
    expect(rows).toHaveLength(2)
    expect(chips(rows[0]!, 'Sources')).toEqual([NODE, 'Ops lead', 'Mesh operators'])
    expect(chips(rows[0]!, 'Via')).toEqual([NODE])
    expect(chips(rows[1]!, 'Sources')).toEqual(['All nodes'])
    expect(chips(rows[1]!, 'Via')).toEqual(['All nodes'])
    expect(document.body.textContent).not.toContain('[object Object]')
    expectNothingRaised()
    w.unmount()
  })

  it('opens the editor with every picker on the value the rule holds', async () => {
    mockApi.get.mockResolvedValue(listOf([namedRule()]))
    const w = mountSection()
    await flushPromises()
    await openEditor(0)
    expect(dialog().textContent).toContain('Edit overlay rule')
    expect(pickers()).toEqual([
      'Node',
      NODE,
      'User',
      'Ops lead',
      'Role',
      'Mesh operators',
      'Node',
      NODE,
      'TCP',
    ])
    expectNothingRaised()
    w.unmount()
  })

  it('saves what it opened: the same kinds and ids, in order', async () => {
    const rule = namedRule()
    mockApi.get.mockResolvedValue(listOf([rule]))
    mockApi.put.mockResolvedValue(rule)
    const w = mountSection()
    await flushPromises()
    await openEditor(0)
    const save = [...dialog().querySelectorAll('button')].find(
      (b) => b.textContent?.trim() === 'Save changes',
    )
    if (!save) throw new Error('the editor has no Save button')
    save.click()
    await flushPromises()
    expect(mockApi.put).toHaveBeenCalledTimes(1)
    const [path, body] = mockApi.put.mock.calls[0]! as [string, Record<string, unknown>]
    expect(path).toBe(`/tenant/${TENANT_ID}/overlay-acl/${RULE_ID}`)
    expect(body.sources).toEqual(rule.sources)
    expect(body.via).toEqual(rule.via)
    expect(body.destinations).toEqual(rule.destinations)
    expect(document.querySelector('.v-overlay--active .v-alert')).toBeNull()
    expectNothingRaised()
    w.unmount()
  })
})
