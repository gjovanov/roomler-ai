// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4248 — the Overlay ACL tab follows its org.
//
// The ACL page is a child route (`network-acl`, `props: true`), so a change of
// the tenant id in the URL reuses the mounted section and only its `tenantId`
// prop changes. The tab loaded once, from its setup, and `useNodeLabel`'s
// `ensureLoaded` loaded the devices and tunnel clients its node labels read
// only while their store was empty. After a push from org A's ACL page to org
// B's, it kept org A's rules, posture, nodes, roles and people.
//
// Every store is real here and the API answers per org, so what the tab shows
// is what each store holds for the org it was asked for. The section's own
// spec (OverlayAclSection.spec.ts) stubs the picker stores with fixed rows,
// which cannot say whose rows they are.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi, type MockInstance } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import OverlayAclSection from '@/components/admin/OverlayAclSection.vue'
import type { OverlayAclMode, OverlayPolicy } from '@/stores/overlayAcl'
import { useAgentStore } from '@/stores/agents'
import { useTunnelClientStore } from '@/stores/tunnelClients'
import { useOverlayRoutesStore } from '@/stores/overlayRoutes'
import { useRoleStore } from '@/stores/role'
import { useMembersStore } from '@/stores/members'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const vuetify = createVuetify({ components, directives })

/** A distinct 24-hex id. */
const hex = (n: number): string => n.toString(16).padStart(24, '0')

const ORG_A = hex(0xa000)
const ORG_B = hex(0xb000)

/** One org's rows: `count` rules, the first naming the org's own node, person
 *  and role; its posture; the picker rows each store answers. */
function makeOrg(tenant: string, tag: string, count: number, mode: OverlayAclMode, ip: string) {
  const base = parseInt(tenant, 16)
  const ids = {
    user: hex(base + 1),
    role: hex(base + 2),
    client: hex(base + 3),
    agent: hex(base + 4),
    node: hex(base + 6),
  }
  const rules: OverlayPolicy[] = []
  for (let i = 0; i < count; i++) {
    const naming = i === 0
    rules.push({
      id: hex(base + 0x10 + i),
      tenant_id: tenant,
      name: `${tag} rule ${i + 1}`,
      enabled: true,
      sources: naming
        ? [
            { kind: 'node_id', id: ids.node },
            { kind: 'user_id', id: ids.user },
            { kind: 'role_id', id: ids.role },
          ]
        : [{ kind: 'all_nodes' }],
      via: naming ? [{ kind: 'node_id', id: ids.node }] : [{ kind: 'all_nodes' }],
      destinations: [{ cidr: '192.0.2.0/24', port_range: { low: 1, high: 65535 }, proto: 'any' }],
      created_at: '2026-11-21T00:00:00Z',
      updated_at: '2026-11-21T00:00:00Z',
    })
  }
  const slug = tag.toLowerCase().replace(/ /g, '-')
  return {
    ...ids,
    tag,
    ip,
    mode,
    rules,
    nodes: [
      {
        id: ids.node,
        name: `${slug}-box`,
        overlay_ip: ip,
        kind: 'agent',
        advertised_routes: [],
        approved_routes: [],
        is_exit_node: false,
        can_be_exit_node: false,
        online: true,
        will_rejoin: true,
        last_seen_at: '2026-11-21T00:00:00Z',
        agent_id: ids.agent,
      },
    ],
    agents: [{ id: ids.agent, name: `${slug}-box`, display_name: `${tag} box` }],
    clients: [{ id: ids.client, name: `${slug}-laptop`, display_name: `${tag} laptop` }],
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

// Org B enforces three rules; org A has one, with enforcement off.
const ORGS: Record<string, ReturnType<typeof makeOrg>> = {
  [ORG_A]: makeOrg(ORG_A, 'Org A', 1, 'off', '100.64.0.10'),
  [ORG_B]: makeOrg(ORG_B, 'Org B', 3, 'enforce', '100.64.0.20'),
}

/** What the server answers for a GET, per org. */
function answer(path: string): unknown {
  const m = /^\/tenant\/([0-9a-f]{24})\/(.*)$/.exec(path)
  const org = m ? ORGS[m[1]!] : undefined
  if (!m || !org) throw new Error(`unexpected GET ${path}`)
  const rest = m[2]!
  const page = <T>(items: T[]) => ({ items, total: items.length, page: 1, per_page: 100 })
  if (rest.startsWith('overlay-acl?')) return { ...page(org.rules), mode: org.mode }
  if (rest === 'overlay-node') return { items: org.nodes }
  if (rest.startsWith('agent?')) return { ...page(org.agents), total_pages: 1 }
  if (rest.startsWith('tunnel-client?')) return { ...page(org.clients), total_pages: 1 }
  if (rest === 'role') return org.roles
  if (rest.startsWith('member?')) return { ...page(org.members), per_page: 25, total_pages: 1 }
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
  return mount(OverlayAclSection, {
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

/** The rule names the table lists, in order. */
function rowNames(): string[] {
  return [...document.querySelectorAll('tbody tr td[data-label="Name"] .font-weight-medium')].map(
    (c) => c.textContent?.trim() ?? '',
  )
}

/** The posture the Enforcement picker shows. */
function shownMode(w: VueWrapper): unknown {
  return w
    .findAllComponents(components.VSelect)
    .find((s) => s.props('label') === 'Enforcement')
    ?.props('modelValue')
}

async function openEditor(row: number) {
  const pencil = document.querySelectorAll('tbody tr')[row]?.querySelector('.mdi-pencil')
  const button = pencil?.closest('button')
  if (!button) throw new Error(`row ${row} has no edit button`)
  button.click()
  await flushPromises()
}

/** The editor's pickers of a row by id, by their label (the kind pickers carry
 *  none; the posture's is "Enforcement" and the destinations' "Proto"). */
const ID_PICKERS = ['Node', 'Role', 'User']

/** What each id picker of the open editor offers: `<label>: <id> (<title>)`.
 *  The editor is teleported, so the pickers are read from the component tree,
 *  not the section's DOM. */
function pickerOffers(w: VueWrapper): string[] {
  return w
    .findAllComponents(components.VSelect)
    .filter((s) => ID_PICKERS.includes(s.props('label') as string))
    .map((s) => {
      const items = s.props('items') as { title: string; value: string }[]
      return `${s.props('label')}: ${items.map((i) => `${i.value} (${i.title})`).join(', ')}`
    })
    .sort()
}

/** Whose rows each store the tab reads holds. */
function storeRows() {
  return {
    nodes: useOverlayRoutesStore().nodes.map((n) => n.id),
    agents: useAgentStore().agents.map((a) => a.id),
    clients: useTunnelClientStore().clients.map((c) => c.id),
    roles: useRoleStore().roles.map((r) => r.id),
    members: useMembersStore().items.map((m) => m.user_id),
  }
}

const B = ORGS[ORG_B]!
const B_ROWS = { nodes: [B.node], agents: [B.agent], clients: [B.client], roles: [B.role], members: [B.user] }

describe('OverlayAclSection — the org changes while the tab is open (#4248)', () => {
  it("asks for org B's rules and pickers, and shows org B's rules and posture", async () => {
    const w = mountSection(ORG_A)
    await settle()
    expect(rowNames()).toEqual(['Org A rule 1'])
    expect(shownMode(w)).toBe('off')

    const since = asked().length
    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()

    const after = asked().slice(since)
    expect(after).toEqual(
      expect.arrayContaining([
        `/tenant/${ORG_B}/overlay-acl?page=1&per_page=100`,
        `/tenant/${ORG_B}/overlay-node`,
        `/tenant/${ORG_B}/role`,
        `/tenant/${ORG_B}/member?page=1&per_page=25`,
        `/tenant/${ORG_B}/agent?per_page=100`,
        `/tenant/${ORG_B}/tunnel-client?per_page=100`,
      ]),
    )
    expect(after.filter((p) => p.includes(ORG_A))).toEqual([])
    expect(rowNames()).toEqual(['Org B rule 1', 'Org B rule 2', 'Org B rule 3'])
    expect(shownMode(w)).toBe('enforce')
    expect(storeRows()).toEqual(B_ROWS)
    expectNothingRaised()
    w.unmount()
  })

  it("offers only org B's nodes, roles and people in the editor, its nodes by device name", async () => {
    const w = mountSection(ORG_A)
    await settle()
    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    await openEditor(0)
    const node = `Node: ${B.node} (Org B box — ${B.ip})`
    expect(pickerOffers(w)).toEqual(
      [node, node, `Role: ${B.role} (Org B operators)`, `User: ${B.user} (Org B lead)`].sort(),
    )
    expectNothingRaised()
    w.unmount()
  })

  it("closes an editor open on org A's rule", async () => {
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

  it("drops org A's rules and posture when they answer after the change", async () => {
    const late = hold(getOf(ORG_A, 'overlay-acl?'))
    const w = mountSection(ORG_A)
    await settle()
    expect(late.taken).toBe(true)

    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    late.release()
    await settle()

    expect(rowNames()).toEqual(['Org B rule 1', 'Org B rule 2', 'Org B rule 3'])
    expect(shownMode(w)).toBe('enforce')
    expectNothingRaised()
    w.unmount()
  })

  it("drops org A's nodes, devices, roles and people when they answer after the change", async () => {
    const late = [
      hold(getOf(ORG_A, 'overlay-node')),
      hold(getOf(ORG_A, 'agent?')),
      hold(getOf(ORG_A, 'tunnel-client?')),
      hold(getOf(ORG_A, 'role')),
      hold(getOf(ORG_A, 'member?')),
    ]
    const w = mountSection(ORG_A)
    await settle()
    expect(late.map((h) => h.taken)).toEqual([true, true, true, true, true])

    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    for (const h of late) h.release()
    await settle()

    expect(storeRows()).toEqual(B_ROWS)
    expectNothingRaised()
    w.unmount()
  })
})
