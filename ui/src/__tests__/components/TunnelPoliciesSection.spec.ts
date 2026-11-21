// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3636 — the Tunnel ACL tab names every subject and target a policy names by
// id, and its editor opens and saves such a policy.
//
// The server answers each id as the 24-hex string `docs/api.md` promises (the
// fixture below). Before the fix it sent bson's `{"$oid": …}`: this page
// printed "User [object Object]", its editor threw while it opened
// (`withCurrent()` slices the id) and Save threw before any request
// (`validateForm()` trims it). The editor is a teleported v-dialog, so the
// spec mounts attached to the document and reads the dialog from there: a
// stub that hid the dialog's body would pass on the broken shape too.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi, type MockInstance } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

const ids = vi.hoisted(() => ({
  user: '6a7e1c2d3b4a59687f0e1d2c',
  role: '6a7e1c2d3b4a59687f0e1d2d',
  client: '6a7e1c2d3b4a59687f0e1d2e',
  device: '6a7e1c2d3b4a59687f0e1d2f',
  // Named by the policy, known to no store here (someone beyond page 1 of
  // the members, a device beyond the first page): shown by id prefix.
  strangerUser: '5f0c9b8a7d6e5f4a3b2c1d0e',
  strangerDevice: '4e1d2c3b4a5f6e7d8c9b0a1f',
}))

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))
vi.mock('@/stores/agents', () => ({
  useAgentStore: () => ({
    agents: [{ id: ids.device, name: 'lab-01', display_name: 'Lab box' }],
    fetchAgents: vi.fn(),
  }),
}))
vi.mock('@/stores/tunnelClients', () => ({
  useTunnelClientStore: () => ({
    clients: [{ id: ids.client, name: 'laptop-07', display_name: 'Field laptop' }],
    fetchTunnelClients: vi.fn(),
  }),
}))
vi.mock('@/stores/role', () => ({
  useRoleStore: () => ({
    roles: [{ id: ids.role, name: 'Tunnel operators' }],
    fetchRoles: vi.fn(),
  }),
}))
vi.mock('@/stores/members', () => ({
  useMembersStore: () => ({
    items: [{ user_id: ids.user, display_name: 'Ops lead', nickname: 'ops-lead' }],
    fetchMembers: vi.fn(),
  }),
}))

import TunnelPoliciesSection from '@/components/admin/TunnelPoliciesSection.vue'
import type { TunnelPolicy } from '@/stores/tunnelPolicies'
import { useTenantStore } from '@/stores/tenant'
import { api } from '@/api/client'
import { pagedRoute, rowId } from '../stores/pagedRoute'

const mockApi = vi.mocked(api)
const TENANT_ID = '69a1dbbad2000f26adc875ce'
const POLICY_ID = '6a7e1c2d3b4a59687f0e1d30'
const CATCH_ALL_ID = '6a7e1c2d3b4a59687f0e1d31'
const MANAGE_AGENTS = 1 << 24
const vuetify = createVuetify({ components, directives })

/** An id as the server writes it on the wire: the 24-hex string (#3636). */
const wireId = (hex: string): string => hex

function namedPolicy(): TunnelPolicy {
  return {
    id: POLICY_ID,
    tenant_id: TENANT_ID,
    name: 'ops reach the lab',
    subjects: [
      { kind: 'user_id', id: wireId(ids.user) },
      { kind: 'role_id', id: wireId(ids.role) },
      { kind: 'tunnel_client_id', id: wireId(ids.client) },
      { kind: 'agent_id', id: wireId(ids.device) },
      { kind: 'user_id', id: wireId(ids.strangerUser) },
    ],
    targets: [
      { kind: 'agent_id', id: wireId(ids.device) },
      { kind: 'agent_id', id: wireId(ids.strangerDevice) },
    ],
    allowlist: [
      {
        host_pattern: { kind: 'cidr', value: '192.0.2.1/32' },
        port_range: { low: 9, high: 9 },
        proto: 'tcp',
      },
    ],
    max_concurrent_flows: null,
    max_bytes_per_session: null,
    created_at: '2026-11-20T00:00:00Z',
    updated_at: '2026-11-20T00:00:00Z',
  }
}

function catchAllPolicy(): TunnelPolicy {
  return {
    ...namedPolicy(),
    id: CATCH_ALL_ID,
    name: 'everyone, everywhere',
    subjects: [{ kind: 'all_users' }],
    targets: [{ kind: 'all_agents' }],
  }
}

function listOf(items: TunnelPolicy[]) {
  return { items, total: items.length, page: 1, per_page: 25, total_pages: 1 }
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
  // The caller holds MANAGE_AGENTS, already known: the store asks the server.
  const t = useTenantStore()
  t.isOwner = false
  t.myPermissions = MANAGE_AGENTS
  t.myPermissionsTenantId = TENANT_ID
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
  return mount(TunnelPoliciesSection, {
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

/** The chips of one cell of the policy table, as a person reads them. */
function chips(row: Element, column: 'Subjects' | 'Targets'): string[] {
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

describe('TunnelPoliciesSection — a policy that names subjects and targets by id (#3636)', () => {
  it('names each one, and an id it does not know by its prefix', async () => {
    mockApi.get.mockResolvedValue(listOf([namedPolicy(), catchAllPolicy()]))
    const w = mountSection()
    await flushPromises()
    expect(mockApi.get).toHaveBeenCalledWith(
      `/tenant/${TENANT_ID}/tunnel-policy?page=1&per_page=100`,
    )
    const rows = [...document.querySelectorAll('tbody tr')]
    expect(rows).toHaveLength(2)
    expect(chips(rows[0]!, 'Subjects')).toEqual([
      'User Ops lead',
      'Role Tunnel operators',
      'Client Field laptop',
      'Agent Lab box',
      'User 5f0c9b…',
    ])
    expect(chips(rows[0]!, 'Targets')).toEqual(['Agent Lab box', 'Agent 4e1d2c…'])
    expect(chips(rows[1]!, 'Subjects')).toEqual(['All users'])
    expect(chips(rows[1]!, 'Targets')).toEqual(['All agents'])
    expect(document.body.textContent).not.toContain('[object Object]')
    expectNothingRaised()
    w.unmount()
  })

  it('opens the editor with every picker on the value the policy holds', async () => {
    mockApi.get.mockResolvedValue(listOf([namedPolicy()]))
    const w = mountSection()
    await flushPromises()
    await openEditor(0)
    expect(dialog().textContent).toContain('Edit tunnel policy')
    expect(pickers()).toEqual([
      'User',
      'Ops lead',
      'Role',
      'Tunnel operators',
      'Tunnel client',
      'Field laptop',
      'Agent',
      'Lab box',
      'User',
      '5f0c9b…',
      'Agent',
      'Lab box',
      'Agent',
      '4e1d2c…',
      'CIDR',
      'TCP',
    ])
    expectNothingRaised()
    w.unmount()
  })

  it('saves what it opened: the same kinds and ids, in order', async () => {
    const policy = namedPolicy()
    mockApi.get.mockResolvedValue(listOf([policy]))
    mockApi.put.mockResolvedValue(policy)
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
    expect(path).toBe(`/tenant/${TENANT_ID}/tunnel-policy/${POLICY_ID}`)
    expect(body.subjects).toEqual(policy.subjects)
    expect(body.targets).toEqual(policy.targets)
    expect(body.allowlist).toEqual(policy.allowlist)
    expect(document.querySelector('.v-overlay--active .v-alert')).toBeNull()
    expectNothingRaised()
    w.unmount()
  })
})

// #4188 — the tab lists every page the route answers, up to the store's cap of
// 50 pages. A route that serves one row a page reaches that cap at 50 rows.
describe('TunnelPoliciesSection — a list longer than one page', () => {
  const row = (i: number): TunnelPolicy => ({
    ...catchAllPolicy(),
    id: rowId(i),
    name: `policy-${i}`,
  })
  const note = () => document.querySelector('[data-testid="tunnel-acl-truncated"]')

  it('lists the rows of every page, with no note that the list was cut', async () => {
    mockApi.get.mockImplementation(
      pagedRoute({ total: 3, row, totalPages: true, perPageCap: 1 }),
    )
    const w = mountSection()
    await flushPromises()
    expect(mockApi.get).toHaveBeenCalledTimes(3)
    expect(document.querySelectorAll('tbody tr')).toHaveLength(3)
    expect(note()).toBeNull()
    expectNothingRaised()
    w.unmount()
  })

  it('says how much of the list it shows when the page cap cut it', async () => {
    mockApi.get.mockImplementation(
      pagedRoute({ total: 51, row, totalPages: true, perPageCap: 1 }),
    )
    const w = mountSection()
    await flushPromises()
    expect(document.querySelectorAll('tbody tr')).toHaveLength(50)
    expect(note()?.textContent?.replace(/\s+/g, ' ').trim()).toBe(
      "Showing the first 50 of this organization's 51 tunnel policies.",
    )
    expectNothingRaised()
    w.unmount()
  })
})
