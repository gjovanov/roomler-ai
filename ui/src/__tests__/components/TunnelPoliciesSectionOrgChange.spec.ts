// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4248 — the Tunnel ACL tab follows its org.
//
// The ACL page is a child route (`network-acl`, `props: true`), so a change of
// the tenant id in the URL reuses the mounted section and only its `tenantId`
// prop changes. The tab loaded once, in `onMounted`, and loaded the devices
// and tunnel clients only while their store was empty. After a push from org
// A's ACL page to org B's, it showed org A's policies (or none) under org B's
// URL, sent no GET for org B's list, and its pickers offered org A's devices,
// clients, roles and people.
//
// Every store is real here and the API answers per org, so what the tab shows
// is what each store holds for the org it was asked for. The section's own
// spec (TunnelPoliciesSection.spec.ts) stubs the picker stores with fixed
// rows, which cannot say whose rows they are.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi, type MockInstance } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import TunnelPoliciesSection from '@/components/admin/TunnelPoliciesSection.vue'
import type { TunnelPolicy } from '@/stores/tunnelPolicies'
import { useTenantStore } from '@/stores/tenant'
import { useAgentStore } from '@/stores/agents'
import { useTunnelClientStore } from '@/stores/tunnelClients'
import { useRoleStore } from '@/stores/role'
import { useMembersStore } from '@/stores/members'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const MANAGE_AGENTS = 1 << 24
const vuetify = createVuetify({ components, directives })

/** A distinct 24-hex id. */
const hex = (n: number): string => n.toString(16).padStart(24, '0')

const ORG_A = hex(0xa000)
const ORG_B = hex(0xb000)

/** One org's rows: `count` policies, the first naming the org's own user,
 *  role, tunnel client and device; the picker rows each store answers. */
function makeOrg(tenant: string, tag: string, count: number) {
  const base = parseInt(tenant, 16)
  const ids = { user: hex(base + 1), role: hex(base + 2), client: hex(base + 3), agent: hex(base + 4) }
  const policies: TunnelPolicy[] = []
  for (let i = 0; i < count; i++) {
    const naming = i === 0
    policies.push({
      id: hex(base + 0x10 + i),
      tenant_id: tenant,
      name: `${tag} policy ${i + 1}`,
      subjects: naming
        ? [
            { kind: 'user_id', id: ids.user },
            { kind: 'role_id', id: ids.role },
            { kind: 'tunnel_client_id', id: ids.client },
            { kind: 'agent_id', id: ids.agent },
          ]
        : [{ kind: 'all_users' }],
      targets: naming ? [{ kind: 'agent_id', id: ids.agent }] : [{ kind: 'all_agents' }],
      allowlist: [
        {
          host_pattern: { kind: 'cidr', value: '192.0.2.1/32' },
          port_range: { low: 22, high: 22 },
          proto: 'tcp',
        },
      ],
      max_concurrent_flows: null,
      max_bytes_per_session: null,
      created_at: '2026-11-21T00:00:00Z',
      updated_at: '2026-11-21T00:00:00Z',
    })
  }
  return {
    ...ids,
    policies,
    agents: [{ id: ids.agent, name: `${tag} box`.toLowerCase().replace(/ /g, '-'), display_name: `${tag} box` }],
    clients: [{ id: ids.client, name: `${tag} laptop`.toLowerCase().replace(/ /g, '-'), display_name: `${tag} laptop` }],
    roles: [{ id: ids.role, name: `${tag} operators` }],
    members: [
      {
        id: hex(base + 5),
        user_id: ids.user,
        display_name: `${tag} lead`,
        email: '',
        role_ids: [],
        joined_at: '2026-11-21T00:00:00Z',
      },
    ],
  }
}

// Org B holds five policies, as the org of the field read did.
const ORGS: Record<string, ReturnType<typeof makeOrg>> = {
  [ORG_A]: makeOrg(ORG_A, 'Org A', 1),
  [ORG_B]: makeOrg(ORG_B, 'Org B', 5),
}

/** What the server answers for a GET, per org. */
function answer(path: string): unknown {
  const m = /^\/tenant\/([0-9a-f]{24})\/(.*)$/.exec(path)
  const org = m ? ORGS[m[1]!] : undefined
  if (!m || !org) throw new Error(`unexpected GET ${path}`)
  const rest = m[2]!
  const page = <T>(items: T[]) => ({
    items,
    total: items.length,
    page: 1,
    per_page: 100,
    total_pages: 1,
  })
  if (rest.startsWith('tunnel-policy?')) return page(org.policies)
  if (rest.startsWith('agent?')) return page(org.agents)
  if (rest.startsWith('tunnel-client?')) return page(org.clients)
  if (rest === 'role') return org.roles
  if (rest === 'member/me') return { permissions: MANAGE_AGENTS, is_owner: false }
  if (rest.startsWith('member?')) return { ...page(org.members), per_page: 25 }
  throw new Error(`unexpected GET ${path}`)
}

/** A GET whose answer the test hands over later. */
interface Held {
  match: (path: string) => boolean
  taken: boolean
  release: () => void
}
let held: Held[] = []

function hold(match: (path: string) => boolean): Held {
  const h: Held = {
    match,
    taken: false,
    release: () => {
      throw new Error('the held GET was never sent')
    },
  }
  held.push(h)
  return h
}

/** A GET of `route` (the path after the tenant) for `tenant`. */
const getOf = (tenant: string, route: string) => (path: string) =>
  path.startsWith(`/tenant/${tenant}/${route}`)

/** Every GET sent so far, in order. */
const asked = (): string[] => mockApi.get.mock.calls.map((c) => c[0] as string)

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
  held = []
  mockApi.get.mockImplementation(async (path: string) => {
    const h = held.find((x) => !x.taken && x.match(path))
    if (!h) return answer(path) as never
    h.taken = true
    return new Promise((resolve) => {
      h.release = () => resolve(answer(path) as never)
    })
  })
  // The caller manages devices in org A, already known (AppLayout read it);
  // org B's mask is read when the tab first asks for it.
  const t = useTenantStore()
  t.isOwner = false
  t.myPermissions = MANAGE_AGENTS
  t.myPermissionsTenantId = ORG_A
  raised = []
  consoleError = vi.spyOn(console, 'error')
  consoleWarn = vi.spyOn(console, 'warn')
})

afterEach(() => {
  consoleError.mockRestore()
  consoleWarn.mockRestore()
  document.body.innerHTML = ''
})

function mountSection(tenantId: string) {
  return mount(TunnelPoliciesSection, {
    props: { tenantId },
    global: {
      plugins: [vuetify, pinia],
      config: { errorHandler: (err: unknown) => void raised.push(err) },
    },
    attachTo: document.body,
  })
}

async function settle() {
  await flushPromises()
  await flushPromises()
}

/** Nothing raised while rendering or handling a change, nothing logged. */
function expectNothingRaised() {
  expect(raised.map(String)).toEqual([])
  expect(consoleError).not.toHaveBeenCalled()
  expect(consoleWarn).not.toHaveBeenCalled()
}

/** The policy names the table lists, in order. */
function rowNames(): string[] {
  return [...document.querySelectorAll('tbody tr td[data-label="Name"] .font-weight-medium')].map(
    (c) => c.textContent?.trim() ?? '',
  )
}

async function openEditor(row: number) {
  const pencil = document.querySelectorAll('tbody tr')[row]?.querySelector('.mdi-pencil')
  const button = pencil?.closest('button')
  if (!button) throw new Error(`row ${row} has no edit button`)
  button.click()
  await flushPromises()
}

/** The editor's pickers of a row by id, by their label (the kind pickers carry
 *  none; the allowlist's are "Match" and "Proto"). */
const ID_PICKERS = ['User', 'Role', 'Tunnel client', 'Device']

/** What each id picker of the open editor offers: `<label>: <ids>`. The editor
 *  is teleported, so the pickers are read from the component tree, not the
 *  section's DOM. */
function pickerOffers(w: VueWrapper): string[] {
  return w
    .findAllComponents(components.VSelect)
    .filter((s) => ID_PICKERS.includes(s.props('label') as string))
    .map((s) => {
      const items = s.props('items') as { value: string }[]
      return `${s.props('label')}: ${items.map((i) => i.value).join(',')}`
    })
    .sort()
}

/** Whose rows each picker store holds. */
function storeRows() {
  return {
    agents: useAgentStore().agents.map((a) => a.id),
    clients: useTunnelClientStore().clients.map((c) => c.id),
    roles: useRoleStore().roles.map((r) => r.id),
    members: useMembersStore().items.map((m) => m.user_id),
  }
}

const B = ORGS[ORG_B]!

describe('TunnelPoliciesSection — the org changes while the tab is open (#4248)', () => {
  it("asks for org B's list and pickers, and lists org B's policies", async () => {
    const w = mountSection(ORG_A)
    await settle()
    expect(rowNames()).toEqual(['Org A policy 1'])

    const since = asked().length
    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()

    const after = asked().slice(since)
    expect(after).toEqual(
      expect.arrayContaining([
        `/tenant/${ORG_B}/tunnel-policy?page=1&per_page=100`,
        `/tenant/${ORG_B}/role`,
        `/tenant/${ORG_B}/member?page=1&per_page=25`,
        `/tenant/${ORG_B}/agent?per_page=100`,
        `/tenant/${ORG_B}/tunnel-client?per_page=100`,
      ]),
    )
    expect(after.filter((p) => p.includes(ORG_A))).toEqual([])
    expect(rowNames()).toEqual([
      'Org B policy 1',
      'Org B policy 2',
      'Org B policy 3',
      'Org B policy 4',
      'Org B policy 5',
    ])
    expect(storeRows()).toEqual({
      agents: [B.agent],
      clients: [B.client],
      roles: [B.role],
      members: [B.user],
    })
    expectNothingRaised()
    w.unmount()
  })

  it("offers only org B's devices, clients, roles and people in the editor", async () => {
    const w = mountSection(ORG_A)
    await settle()
    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    await openEditor(0)
    expect(pickerOffers(w)).toEqual(
      [
        `Device: ${B.agent}`,
        `Device: ${B.agent}`,
        `Role: ${B.role}`,
        `Tunnel client: ${B.client}`,
        `User: ${B.user}`,
      ].sort(),
    )
    expectNothingRaised()
    w.unmount()
  })

  it("closes an editor open on org A's policy", async () => {
    const w = mountSection(ORG_A)
    await settle()
    await openEditor(0)
    expect(w.findAllComponents(components.VDialog).map((d) => d.props('modelValue'))).toContain(true)

    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    expect(w.findAllComponents(components.VDialog).map((d) => d.props('modelValue'))).not.toContain(
      true,
    )
    expectNothingRaised()
    w.unmount()
  })

  it("drops org A's list when it answers after the change", async () => {
    const late = hold(getOf(ORG_A, 'tunnel-policy?'))
    const w = mountSection(ORG_A)
    await settle()
    expect(late.taken).toBe(true)

    const since = asked().length
    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    late.release()
    await settle()

    expect(rowNames()).toEqual([
      'Org B policy 1',
      'Org B policy 2',
      'Org B policy 3',
      'Org B policy 4',
      'Org B policy 5',
    ])
    // Nothing is asked for org A once the tab is org B's: its late list must
    // not go on to load org A's pickers.
    expect(asked().slice(since).filter((p) => p.includes(ORG_A))).toEqual([])
    expect(storeRows()).toEqual({
      agents: [B.agent],
      clients: [B.client],
      roles: [B.role],
      members: [B.user],
    })
    expectNothingRaised()
    w.unmount()
  })

  it("drops org A's picker rows when they answer after the change", async () => {
    const late = [
      hold(getOf(ORG_A, 'agent?')),
      hold(getOf(ORG_A, 'tunnel-client?')),
      hold(getOf(ORG_A, 'role')),
      hold(getOf(ORG_A, 'member?')),
    ]
    const w = mountSection(ORG_A)
    await settle()
    expect(late.map((h) => h.taken)).toEqual([true, true, true, true])

    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    for (const h of late) h.release()
    await settle()

    expect(storeRows()).toEqual({
      agents: [B.agent],
      clients: [B.client],
      roles: [B.role],
      members: [B.user],
    })
    expectNothingRaised()
    w.unmount()
  })
})
