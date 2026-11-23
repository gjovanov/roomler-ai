// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4257 — the org dashboard, the Rooms page and Explore follow the org in
// their URL. No route remounts on the org (`meta.remountOn` names only
// userId, roomId and agentId), so a link, back or forward from one org's page
// to another org's REUSES the page, and each page loaded its org once, on
// mount. The org switcher, used from another page, mounts the dashboard fresh
// on stores that still hold the previous org's rows.
//
// These run the REAL views through KeyedRouterView, on route records that
// carry the app's own meta, under a stand-in for the shell's own reloads on a
// change of org (AppLayout: the rooms when chat is on, the devices when the
// fleet nav shows). What the shell reloads is not what these test; what it
// leaves to the page is: the tunnel clients, the overview and the mesh, the
// search, a dialog opened under the previous org, and a late answer.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { defineComponent, h, watch } from 'vue'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createI18n } from 'vue-i18n'
import { createMemoryHistory, createRouter, useRoute, type RouteRecordRaw, type Router } from 'vue-router'
import en from '@/locales/en.json'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
  ApiError: class ApiError extends Error {},
}))
// The charts are not what these test: the mesh graph names its nodes, so a
// test can read whose mesh it was handed.
vi.mock('@/components/stats/MeshGraph.vue', async () => {
  const { defineComponent, h } = await import('vue')
  return {
    default: defineComponent({
      name: 'MeshGraph',
      props: { nodes: { type: Array, default: () => [] }, edges: Array, centerName: String },
      setup(props) {
        return () =>
          h(
            'div',
            { 'data-testid': 'mesh-nodes' },
            (props.nodes as Array<{ name: string }>).map((n) => n.name).join(', '),
          )
      },
    }),
  }
})
vi.mock('@/components/stats/TimeSeriesChart.vue', async () => {
  const { defineComponent, h } = await import('vue')
  return { default: defineComponent({ name: 'TimeSeriesChart', render: () => h('div') }) }
})

import appRouter from '@/plugins/router'
import KeyedRouterView from '@/components/layout/KeyedRouterView.vue'
import TenantDashboard from '@/views/dashboard/TenantDashboard.vue'
import RoomList from '@/views/rooms/RoomList.vue'
import ExploreView from '@/views/rooms/ExploreView.vue'
import { useRouteTenantSync } from '@/composables/useRouteTenantSync'
import { useTenantStore } from '@/stores/tenant'
import { useRoomStore } from '@/stores/rooms'
import { useAgentStore } from '@/stores/agents'
import { useTunnelClientStore } from '@/stores/tunnelClients'
import { useStatsStore } from '@/stores/stats'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const vuetify = createVuetify({ components, directives })
const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })

/** A distinct 24-hex id. */
const hex = (n: number): string => n.toString(16).padStart(24, '0')

const ORG_A = hex(0xa000)
const ORG_B = hex(0xb000)

function room(id: string, name: string, messages: number, inCall = 0) {
  return {
    id,
    name,
    is_open: true,
    member_count: 1,
    message_count: messages,
    conference_status: inCall ? 'in_progress' : undefined,
    participant_count: inCall,
  }
}

/** One org's rows, each named after the org. */
const ORGS: Record<
  string,
  {
    tag: string
    name: string
    rooms: ReturnType<typeof room>[]
    machines: { online: number; total: number }
    clients: Array<{ id: string; name: string; status: string }>
    box: string
  }
> = {
  [ORG_A]: {
    tag: 'Org A',
    name: 'Org A',
    rooms: [
      room(hex(0xa011), 'Org A general', 10, 2),
      room(hex(0xa012), 'Org A eng', 15, 1),
      room(hex(0xa013), 'Org A ops', 5),
    ],
    machines: { online: 7, total: 9 },
    clients: [
      { id: hex(0xa021), name: 'org-a-1', status: 'online' },
      { id: hex(0xa022), name: 'org-a-2', status: 'online' },
      { id: hex(0xa023), name: 'org-a-3', status: 'online' },
      { id: hex(0xa024), name: 'org-a-4', status: 'online' },
      { id: hex(0xa025), name: 'org-a-5', status: 'offline' },
    ],
    box: 'Org A box',
  },
  [ORG_B]: {
    tag: 'Org B',
    name: 'Org B',
    rooms: [room(hex(0xb011), 'Org B general', 4)],
    machines: { online: 2, total: 3 },
    clients: [{ id: hex(0xb021), name: 'org-b-1', status: 'online' }],
    box: 'Org B box',
  },
}
const A_ENG_ROOM = hex(0xa012)

/** What each org's dashboard tiles read. */
const TILES = ['Devices Online', 'Tunnels', 'Rooms', 'Active Calls', 'Messages'] as const
const SHOWS: Record<string, Record<(typeof TILES)[number], string>> = {
  [ORG_A]: { 'Devices Online': '7/9', Tunnels: '4/5', Rooms: '3', 'Active Calls': '2', Messages: '30' },
  [ORG_B]: { 'Devices Online': '2/3', Tunnels: '1/1', Rooms: '1', 'Active Calls': '0', Messages: '4' },
}
/** The tiles while the org's rows have not landed: nothing of any org. */
const NOTHING_YET = { 'Devices Online': '0', Tunnels: '0/0', Rooms: '0', 'Active Calls': '0', Messages: '0' }

/** GETs no answer below covers: none is expected. */
let unexpected: string[] = []

/** What the server answers for a GET, per org. */
function answer(path: string): unknown {
  const m = /^\/tenant\/([0-9a-f]{24})\/([^?]*)(?:\?(.*))?$/.exec(path)
  const org = m ? ORGS[m[1]!] : undefined
  if (!m || !org) {
    unexpected.push(path)
    return []
  }
  const q = new URLSearchParams(m[3] ?? '')
  switch (m[2]) {
    case 'room':
      return org.rooms
    case 'room/explore': {
      const name = `${org.tag} ${q.get('q') || 'lobby'}`
      return [{ id: name, name, is_open: true, member_count: 1, message_count: 0 }]
    }
    case 'agent':
      return {
        items: [{ id: hex(parseInt(m[1]!, 16) + 4), name: 'box', display_name: org.box, is_online: true }],
        total: 1,
        page: 1,
        per_page: 100,
        total_pages: 1,
      }
    case 'tunnel-client':
      return { items: org.clients, total: org.clients.length }
    case 'stats/overview':
      return { enabled: true, machines: org.machines }
    case 'stats/mesh':
      return { enabled: true, nodes: [{ id: `${org.tag} node`, name: org.box }], agents: [], edges: [] }
  }
  unexpected.push(path)
  return []
}

/** Every matching GET waits until the test opens the gate. */
interface Gate {
  match: (path: string) => boolean
  sent: number
  waiting: Array<() => void>
  isOpen: boolean
  open: () => void
}
let gates: Gate[] = []

function gate(match: (path: string) => boolean): Gate {
  const g: Gate = {
    match,
    sent: 0,
    waiting: [],
    isOpen: false,
    open: () => {
      expect(g.sent, 'a GET the gate holds was sent').toBeGreaterThan(0)
      g.isOpen = true
      for (const go of g.waiting.splice(0)) go()
    },
  }
  gates.push(g)
  return g
}

/** A GET of `route` (the path after the tenant, up to any query) for `tenant`. */
const getOf = (tenant: string, route: string) => (path: string) =>
  path.split('?')[0] === `/tenant/${tenant}/${route}`

/** Every GET sent so far, in order. */
const asked = (): string[] => mockApi.get.mock.calls.map((c) => c[0] as string)
const timesAsked = (path: string) => asked().filter((p) => p === path).length

/** The app's own record for the named route: a record that gains a key on the
 *  org, or loses its module, changes what these see. */
function realRecord(name: string): RouteRecordRaw {
  let found: RouteRecordRaw | undefined
  const walk = (rs: readonly RouteRecordRaw[]) => {
    for (const r of rs) {
      if (r.name === name) found = r
      if (r.children) walk(r.children)
    }
  }
  walk(appRouter.options.routes)
  expect(found, `the app has a ${name} route`).toBeTruthy()
  return found!
}

const Blank = defineComponent({ render: () => h('div') })

function makeRouter(): Router {
  const page = (name: string, component: unknown): RouteRecordRaw => {
    const r = realRecord(name)
    return { path: r.path, name, component, meta: r.meta } as RouteRecordRaw
  }
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      {
        path: '/tenant/:tenantId',
        children: [
          page('tenant-dashboard', TenantDashboard),
          page('rooms', RoomList),
          page('explore', ExploreView),
          { path: 'tutorial', name: 'tutorial', component: Blank },
        ],
      },
      { path: '/:rest(.*)*', component: Blank },
    ],
  })
}

/** The shell's own reloads on a change of org, as AppLayout does them: the
 *  route's org into `tenantStore.current` (`useRouteTenantSync`), the rooms
 *  on a change of `current` when chat is on, the devices on every change of
 *  the org when the fleet nav shows (`components/layout/AppLayout.vue`, the
 *  rooms watch on `tenantStore.current` and the device watch below it). */
const Shell = defineComponent({
  name: 'ShellStandIn',
  setup() {
    const route = useRoute()
    const tenantStore = useTenantStore()
    const roomStore = useRoomStore()
    const agentStore = useAgentStore()
    useRouteTenantSync(route)
    watch(
      () => tenantStore.current?.id ?? null,
      (tid, prev) => {
        if (tid && prev !== undefined && prev !== tid) void roomStore.fetchRooms(tid).catch(() => {})
      },
      { immediate: true },
    )
    watch(
      () => (route.params.tenantId as string | undefined) || tenantStore.current?.id || '',
      (tid) => {
        if (tid) void agentStore.fetchAgents(tid).catch(() => {})
      },
      { immediate: true },
    )
    return () => h(KeyedRouterView)
  },
})

beforeAll(() => {
  // Vuetify overlays measure themselves against the viewport; jsdom has
  // neither a ResizeObserver nor a visualViewport.
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
let router: Router
let wrapper: VueWrapper | null = null
let raised: unknown[]

beforeEach(() => {
  pinia = createPinia()
  setActivePinia(pinia)
  vi.clearAllMocks()
  gates = []
  unexpected = []
  raised = []
  mockApi.get.mockImplementation(async (path: string) => {
    const g = gates.find((x) => !x.isOpen && x.match(path))
    if (!g) return answer(path) as never
    g.sent++
    return new Promise((resolve) => {
      g.waiting.push(() => resolve(answer(path) as never))
    })
  })
  // The caller owns both orgs: every tile and section is theirs to see.
  const t = useTenantStore()
  t.tenants = [
    { id: ORG_A, name: 'Org A', slug: 'org-a' },
    { id: ORG_B, name: 'Org B', slug: 'org-b' },
  ] as never
  t.isOwner = true
  t.myPermissions = 0
  router = makeRouter()
})

afterEach(() => {
  wrapper?.unmount()
  wrapper = null
  document.body.innerHTML = ''
})

async function settle() {
  for (let i = 0; i < 3; i++) {
    await flushPromises()
    await new Promise((r) => setTimeout(r, 0))
  }
}

/** The app at `path`, as a person lands there. */
async function open(path: string) {
  await router.push(path)
  await router.isReady()
  wrapper = mount(Shell, {
    global: {
      plugins: [vuetify, i18n, pinia, router],
      config: { errorHandler: (err: unknown) => void raised.push(err) },
    },
    attachTo: document.body,
  })
  await settle()
}

/** A link, in place. */
async function go(path: string) {
  await router.push(path)
  await settle()
}

/** Nothing raised while rendering or handling a change, and no GET the
 *  server would not answer. */
function expectNothingRaised() {
  expect(raised.map(String)).toEqual([])
  expect(unexpected).toEqual([])
}

function tile(label: string): string | undefined {
  for (const t of document.body.querySelectorAll('.v-card-text')) {
    if (t.querySelector('.text-subtitle-2')?.textContent?.trim() === label) {
      return t.querySelector('.text-h4')?.textContent?.trim()
    }
  }
  return undefined
}
const tiles = () => Object.fromEntries(TILES.map((l) => [l, tile(l)]))
const mesh = () => document.body.querySelector('[data-testid="mesh-nodes"]')?.textContent?.trim() ?? ''

describe('The org dashboard follows its org (#4257)', () => {
  it("a link to org B's dashboard reuses the page: it asks for org B's tunnel clients, overview and mesh, shows org B's, and back shows org A's again", async () => {
    await open(`/tenant/${ORG_A}`)
    expect(tiles()).toEqual(SHOWS[ORG_A])
    expect(mesh()).toBe('Org A box')
    const page = wrapper!.findComponent(TenantDashboard).vm.$.uid

    await go(`/tenant/${ORG_B}`)
    expect(wrapper!.findComponent(TenantDashboard).vm.$.uid, 'the page is reused').toBe(page)
    expect(timesAsked(`/tenant/${ORG_B}/tunnel-client?per_page=100`)).toBe(1)
    expect(timesAsked(`/tenant/${ORG_B}/stats/overview`)).toBe(1)
    expect(timesAsked(`/tenant/${ORG_B}/stats/mesh`)).toBe(1)
    // The shell reloaded the rooms and the devices; the page asks for no
    // second copy of either.
    expect(timesAsked(`/tenant/${ORG_B}/room`)).toBe(1)
    expect(timesAsked(`/tenant/${ORG_B}/agent?per_page=100`)).toBe(1)
    expect(tiles()).toEqual(SHOWS[ORG_B])
    expect(mesh()).toBe('Org B box')

    router.back()
    await settle()
    expect(tiles()).toEqual(SHOWS[ORG_A])
    expect(mesh()).toBe('Org A box')
    expectNothingRaised()
  })

  it("drops org A's overview, mesh and tunnel clients when they answer after the change", async () => {
    const late = gate(
      (p) =>
        getOf(ORG_A, 'stats/overview')(p) || getOf(ORG_A, 'stats/mesh')(p) || getOf(ORG_A, 'tunnel-client')(p),
    )
    await open(`/tenant/${ORG_A}`)
    await go(`/tenant/${ORG_B}`)
    expect(tiles()).toEqual(SHOWS[ORG_B])

    late.open()
    await settle()
    expect(tiles()).toEqual(SHOWS[ORG_B])
    expect(mesh()).toBe('Org B box')
    expectNothingRaised()
  })

  it("mounted fresh by the org switcher from another org page, on stores that still hold org A's rows: it shows none of them while org B's load", async () => {
    // Org A's Rooms page, after org A's dashboard filled the other stores.
    await open(`/tenant/${ORG_A}/rooms`)
    await useTunnelClientStore().fetchTunnelClients(ORG_A)
    await useStatsStore().fetchOverview(ORG_A)
    await useStatsStore().fetchMesh(ORG_A)

    const orgB = gate((p) => p.startsWith(`/tenant/${ORG_B}/`))
    // What the org switcher does (AppLayout `selectTenant`).
    useTenantStore().setCurrent({ id: ORG_B, name: 'Org B', slug: 'org-b' } as never)
    await go(`/tenant/${ORG_B}`)
    expect(wrapper!.findComponent(TenantDashboard).exists()).toBe(true)
    expect(tiles()).toEqual(NOTHING_YET)
    expect(mesh()).toBe('')

    orgB.open()
    await settle()
    expect(tiles()).toEqual(SHOWS[ORG_B])
    expect(mesh()).toBe('Org B box')
    expectNothingRaised()
  })
})

/** The names the room list shows. */
const roomRows = () =>
  [...document.body.querySelectorAll('.v-list-item-title')].map((e) => e.textContent?.trim() ?? '')
const activeDialog = () => document.body.querySelector<HTMLElement>('.v-dialog.v-overlay--active')

async function findByText(selector: string, text: string, tries = 30): Promise<HTMLElement | null> {
  for (let i = 0; i < tries; i++) {
    const el = [...document.body.querySelectorAll<HTMLElement>(selector)].find(
      (e) => e.textContent?.trim() === text,
    )
    if (el) return el
    await flushPromises()
    await new Promise((r) => setTimeout(r, 10))
  }
  return null
}

/** The row menu's "Create Sub-Room", as a person opens it on `roomName`. */
async function openCreateChild(roomName: string) {
  const row = [...document.body.querySelectorAll<HTMLElement>('.v-list-item')].find(
    (r) => r.querySelector('.v-list-item-title')?.textContent?.trim() === roomName,
  )
  expect(row, `a row for ${roomName}`).toBeTruthy()
  row!.querySelector('.mdi-dots-vertical')!.closest('button')!.click()
  const item = await findByText('.v-list-item', 'Create Sub-Room')
  expect(item, 'the row menu offers a sub-room').toBeTruthy()
  item!.click()
  await settle()
}

describe('The Rooms page follows its org (#4257)', () => {
  it('closes a create dialog opened under org A, and forgets the parent it named', async () => {
    await open(`/tenant/${ORG_A}/rooms`)
    expect(roomRows()).toEqual(['Org A general', 'Org A eng', 'Org A ops'])
    await openCreateChild('Org A eng')
    expect(activeDialog()?.textContent).toContain('Org A eng')

    await go(`/tenant/${ORG_B}/rooms`)
    expect(activeDialog(), 'the dialog opened under org A is closed').toBeNull()
    expect(roomRows()).toEqual(['Org B general'])
    // The shell reloaded the rooms; the page asks for no second copy.
    expect(timesAsked(`/tenant/${ORG_B}/room`)).toBe(1)

    const create = await findByText('button', 'Create Room')
    create!.click()
    await settle()
    const dialog = activeDialog()
    expect(dialog, 'a new create dialog opens').not.toBeNull()
    expect(dialog!.textContent).not.toContain('Org A')
    expect(dialog!.textContent).not.toContain(A_ENG_ROOM)
    expectNothingRaised()
  })

  it("drops org A's rooms when they answer after the change", async () => {
    const late = gate(getOf(ORG_A, 'room'))
    await open(`/tenant/${ORG_A}/rooms`)
    await go(`/tenant/${ORG_B}/rooms`)
    expect(roomRows()).toEqual(['Org B general'])

    late.open()
    await settle()
    expect(roomRows()).toEqual(['Org B general'])
    expectNothingRaised()
  })
})

/** The rooms Explore lists. */
const results = () =>
  [...document.body.querySelectorAll('.v-card-title .text-truncate')].map((e) => e.textContent?.trim() ?? '')
const spinning = () => document.body.querySelector('.v-progress-circular') !== null

/** A query typed into Explore's search, past its debounce. */
async function typeQuery(q: string) {
  const input = document.body.querySelector<HTMLInputElement>('input')!
  input.value = q
  input.dispatchEvent(new Event('input'))
  await new Promise((r) => setTimeout(r, 350))
  await settle()
}

describe('Explore follows its org (#4257)', () => {
  it("runs the search again for org B, and lists org B's open rooms", async () => {
    await open(`/tenant/${ORG_A}/explore`)
    expect(results()).toEqual(['Org A lobby'])

    await go(`/tenant/${ORG_B}/explore`)
    expect(timesAsked(`/tenant/${ORG_B}/room/explore?q=`)).toBe(1)
    expect(results()).toEqual(['Org B lobby'])
    expect(spinning()).toBe(false)
    expectNothingRaised()
  })

  it("drops org A's search when it answers after the change", async () => {
    const late = gate(getOf(ORG_A, 'room/explore'))
    await open(`/tenant/${ORG_A}/explore`)
    await go(`/tenant/${ORG_B}/explore`)
    expect(results()).toEqual(['Org B lobby'])

    late.open()
    await settle()
    expect(results()).toEqual(['Org B lobby'])
    expect(spinning()).toBe(false)
    expectNothingRaised()
  })

  it("drops an older query's answer that lands after a newer one's", async () => {
    await open(`/tenant/${ORG_A}/explore`)
    const older = gate((p) => p === `/tenant/${ORG_A}/room/explore?q=old`)
    await typeQuery('old')
    await typeQuery('new')
    expect(results()).toEqual(['Org A new'])

    older.open()
    await settle()
    expect(results()).toEqual(['Org A new'])
    expect(spinning()).toBe(false)
    expectNothingRaised()
  })
})
