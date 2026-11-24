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
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn(), upload: vi.fn() },
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
import FilesBrowser from '@/views/files/FilesBrowser.vue'
import InviteManageView from '@/views/invite/InviteManageView.vue'
import BatchInviteDialog from '@/components/invite/BatchInviteDialog.vue'
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

function fileRow(id: string, filename: string) {
  return {
    id,
    tenant_id: '',
    filename,
    content_type: 'text/plain',
    size: 2048,
    uploaded_by: 'u',
    created_at: '2026-11-01T00:00:00Z',
  }
}
function inviteRow(id: string, tenant: string, email: string) {
  return {
    id,
    code: `code${id.slice(-4)}`,
    tenant_id: tenant,
    inviter_id: 'u',
    target_email: email,
    use_count: 0,
    status: 'active',
    assign_role_ids: [],
    created_at: '2026-11-01T00:00:00Z',
  }
}
function roleRow(id: string, tenant: string, name: string) {
  return { id, tenant_id: tenant, name, position: 10, permissions: 0, is_default: false, is_managed: false, is_mentionable: false }
}

/** Each org's grids: its files, a room's files, its invites and its roles.
 *  Org A's files and invites fill three pages, so a test can leave the first. */
const GRIDS: Record<
  string,
  {
    files: ReturnType<typeof fileRow>[]
    fileTotal: number
    roomFiles: Record<string, ReturnType<typeof fileRow>[]>
    invites: ReturnType<typeof inviteRow>[]
    inviteTotal: number
    roles: ReturnType<typeof roleRow>[]
  }
> = {
  [ORG_A]: {
    files: [fileRow(hex(0xa031), 'Org A plan.txt')],
    fileTotal: 60,
    roomFiles: { [A_ENG_ROOM]: [fileRow(hex(0xa032), 'Org A eng diagram.txt')] },
    invites: [inviteRow(hex(0xa041), ORG_A, 'guest@org-a.example')],
    inviteTotal: 60,
    roles: [roleRow(hex(0xa002), ORG_A, 'Org A operators')],
  },
  [ORG_B]: {
    files: [fileRow(hex(0xb031), 'Org B notes.txt')],
    fileTotal: 1,
    roomFiles: {},
    invites: [inviteRow(hex(0xb041), ORG_B, 'guest@org-b.example')],
    inviteTotal: 1,
    roles: [roleRow(hex(0xb002), ORG_B, 'Org B operators')],
  },
}
const A_ROLE = hex(0xa002)

/** One page of a server grid. */
function pageOf(items: unknown[], total: number, q: URLSearchParams) {
  const perPage = Number(q.get('per_page') ?? 25)
  return { items, total, page: Number(q.get('page') ?? 1), per_page: perPage, total_pages: Math.ceil(total / perPage) }
}

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
  const grid = GRIDS[m[1]!]!
  const roomFiles = /^room\/([0-9a-f]{24})\/file$/.exec(m[2]!)
  if (roomFiles) {
    const files = grid.roomFiles[roomFiles[1]!]
    if (files) return pageOf(files, files.length, q)
    // A room of another org, asked under this one.
    unexpected.push(path)
    return pageOf([], 0, q)
  }
  switch (m[2]) {
    case 'file':
      return pageOf(grid.files, grid.fileTotal, q)
    case 'invite':
      return pageOf(grid.invites, grid.inviteTotal, q)
    case 'role':
      return grid.roles
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
          page('files', FilesBrowser),
          page('invites', InviteManageView),
          page('room-call', Blank),
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

  // The review's F6: "Start Call" read the page's org after the room's create
  // answered, so a call created in org A and answered after the page moved to
  // org B opened org A's room under org B. A call answered for an org the page
  // has left opens nothing there.
  it('a call started in org A that answers after the change to org B opens no call page under org B', async () => {
    const callRoom = hex(0xca11)
    let releaseCreate: () => void = () => {
      throw new Error('no room create was sent')
    }
    mockApi.post.mockImplementationOnce(async (path: string) => {
      if (path !== `/tenant/${ORG_A}/room`) {
        unexpected.push(`POST ${path}`)
        return {} as never
      }
      await new Promise<void>((r) => (releaseCreate = r))
      return { id: callRoom, tenant_id: ORG_A, name: 'Org A call' } as never
    })
    await open(`/tenant/${ORG_A}`)
    await clickButton('Start Call')

    await go(`/tenant/${ORG_B}`)
    releaseCreate()
    await settle()
    expect(router.currentRoute.value.fullPath).toBe(`/tenant/${ORG_B}`)
    expect(wrapper!.findComponent(TenantDashboard).exists()).toBe(true)
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

// ── the paging grids (slice 5) ──────────────────────────────────────────

/** A grid's search box waits 300 ms after a keystroke, a clear included. */
const debounce = () => new Promise((r) => setTimeout(r, 350))
const pageText = () => wrapper!.text()

async function clickButton(text: string, root: ParentNode = document.body) {
  const b = [...root.querySelectorAll<HTMLElement>('button')].find((x) => x.textContent?.trim() === text)
  expect(b, `a "${text}" button`).toBeTruthy()
  b!.click()
  await settle()
}

async function typeIn(placeholder: string, value: string) {
  const box = document.body.querySelector<HTMLInputElement>(`input[placeholder="${placeholder}"]`)!
  box.value = value
  box.dispatchEvent(new Event('input'))
  await debounce()
  await settle()
  return box
}

/** Under org A, search `q` and go to the grid's second page; then a link to
 *  org B's page. Every GET of org B's grid asks for page 1, unfiltered. */
async function searchPageThenChange(route: string, placeholder: string, q: string) {
  await open(`/tenant/${ORG_A}/${route}`)
  const box = await typeIn(placeholder, q)
  ;(document.body.querySelector('button[aria-label="Next page"]') as HTMLElement).click()
  await settle()
  const forA = asked().filter(getOf(ORG_A, route === 'files' ? 'file' : 'invite'))
  expect(forA.at(-1)).toContain('page=2&')
  expect(forA.at(-1)).toContain(`q=${q}`)
  const since = asked().length

  await go(`/tenant/${ORG_B}/${route}`)
  await debounce()
  await settle()
  const forB = asked().slice(since).filter(getOf(ORG_B, route === 'files' ? 'file' : 'invite'))
  expect(forB.length).toBeGreaterThan(0)
  for (const path of forB) {
    expect(path).toContain('page=1&')
    expect(path).not.toContain('q=')
  }
  expect(box.value).toBe('')
}

describe('The Files page follows its org (#4257)', () => {
  it("a link to org B's files asks for org B's files and lists them", async () => {
    await open(`/tenant/${ORG_A}/files`)
    expect(pageText()).toContain('Org A plan.txt')
    const since = asked().length

    await go(`/tenant/${ORG_B}/files`)
    expect(asked().slice(since).filter(getOf(ORG_B, 'file'))).toHaveLength(1)
    expect(pageText()).toContain('Org B notes.txt')
    expect(pageText()).not.toContain('Org A plan.txt')
    expectNothingRaised()
  })

  it("drops org A's files when they answer after the change", async () => {
    const lateA = gate(getOf(ORG_A, 'file'))
    await open(`/tenant/${ORG_A}/files`)
    await go(`/tenant/${ORG_B}/files`)
    lateA.open()
    await settle()
    expect(pageText()).toContain('Org B notes.txt')
    expect(pageText()).not.toContain('Org A plan.txt')
    expectNothingRaised()
  })

  it('a search and a page chosen under org A do not carry over: org B is asked for its first page, unfiltered', async () => {
    await searchPageThenChange('files', 'Search files', 'plan')
    expectNothingRaised()
  })

  it("the room view under org A goes back to all files: org B's files, never org A's room asked under org B", async () => {
    // The person had opened org A's eng room: the room view lists its files.
    useRoomStore().setCurrent(ORGS[ORG_A]!.rooms[1] as never)
    await open(`/tenant/${ORG_A}/files`)
    await clickButton('Room Files')
    expect(pageText()).toContain('Org A eng diagram.txt')
    const since = asked().length

    await go(`/tenant/${ORG_B}/files`)
    expect(asked().slice(since).filter(getOf(ORG_B, 'file')).length).toBeGreaterThan(0)
    expect(pageText()).toContain('Org B notes.txt')
    expect(pageText()).not.toContain('Org A eng diagram.txt')
    expectNothingRaised()
  })

  it('an upload started under org A sends every file of its batch to org A', async () => {
    useRoomStore().setCurrent(ORGS[ORG_A]!.rooms[1] as never)
    const sent: string[] = []
    let firstAnswers!: () => void
    mockApi.upload.mockImplementation(async (path: string) => {
      sent.push(path)
      if (sent.length === 1) await new Promise<void>((r) => (firstAnswers = r))
      return fileRow(hex(0xa0f0 + sent.length), `upload ${sent.length}.txt`) as never
    })
    await open(`/tenant/${ORG_A}/files`)
    const input = document.body.querySelector<HTMLInputElement>('input[type="file"]')!
    Object.defineProperty(input, 'files', {
      configurable: true,
      value: [new File(['1'], 'one.txt'), new File(['2'], 'two.txt')],
    })
    input.dispatchEvent(new Event('change'))
    await settle()
    expect(sent).toEqual([`/tenant/${ORG_A}/file/upload`])

    await go(`/tenant/${ORG_B}/files`)
    firstAnswers()
    await settle()
    expect(sent).toEqual([`/tenant/${ORG_A}/file/upload`, `/tenant/${ORG_A}/file/upload`])
    expectNothingRaised()
  })
})

describe('The Invites page follows its org (#4257)', () => {
  /** What the server answers for a POST: a create or a batch of org A, held
   *  until `releaseA` when `holdA` is set. */
  let holdA = false
  let releaseA: () => void = () => {
    throw new Error('no POST of org A was held')
  }
  beforeEach(() => {
    holdA = false
    mockApi.post.mockImplementation(async (path: string, body?: unknown) => {
      const m = /^\/tenant\/([0-9a-f]{24})\/invite(\/batch)?$/.exec(path)
      if (!m) {
        unexpected.push(`POST ${path}`)
        return {} as never
      }
      if (holdA && m[1] === ORG_A) await new Promise<void>((r) => (releaseA = r))
      if (!m[2]) {
        const b = body as { target_email?: string }
        return inviteRow(hex(0xc001), m[1]!, b.target_email ?? '') as never
      }
      const rows = (body as { invites: Array<{ target_email: string }> }).invites
      return {
        results: rows.map((r, i) => ({ invite: inviteRow(hex(0xc010 + i), m[1]!, r.target_email), target_email: r.target_email })),
        created: rows.length,
        failed: 0,
      } as never
    })
  })

  it("a link to org B's invites asks for org B's invites and roles, and lists org B's invites", async () => {
    await open(`/tenant/${ORG_A}/invites`)
    expect(pageText()).toContain('guest@org-a.example')
    const since = asked().length

    await go(`/tenant/${ORG_B}/invites`)
    const after = asked().slice(since)
    expect(after.filter(getOf(ORG_B, 'invite'))).toHaveLength(1)
    expect(after).toContain(`/tenant/${ORG_B}/role`)
    expect(pageText()).toContain('guest@org-b.example')
    expect(pageText()).not.toContain('guest@org-a.example')
    expectNothingRaised()
  })

  it("drops org A's invites when they answer after the change", async () => {
    const lateA = gate(getOf(ORG_A, 'invite'))
    await open(`/tenant/${ORG_A}/invites`)
    await go(`/tenant/${ORG_B}/invites`)
    lateA.open()
    await settle()
    expect(pageText()).toContain('guest@org-b.example')
    expect(pageText()).not.toContain('guest@org-a.example')
    expectNothingRaised()
  })

  it('a search and a page chosen under org A do not carry over: org B is asked for its first page, unfiltered', async () => {
    await searchPageThenChange('invites', 'Search code or email', 'guest')
    expectNothingRaised()
  })

  it('closes the create dialog opened under org A; a create from the dialog opened again goes to org B', async () => {
    await open(`/tenant/${ORG_A}/invites`)
    await clickButton('Create Invite')
    expect(activeDialog()?.textContent).toContain('Create Invite')

    await go(`/tenant/${ORG_B}/invites`)
    expect(activeDialog()).toBeNull()
    await clickButton('Create Invite')
    await clickButton('Create', activeDialog()!)
    expect(mockApi.post.mock.calls.map((c) => c[0])).toEqual([`/tenant/${ORG_B}/invite`])
    expectNothingRaised()
  })

  it("closes the batch dialog opened under org A; opened again under org B it holds none of org A's rows and is handed org B's roles", async () => {
    await open(`/tenant/${ORG_A}/invites`)
    await clickButton('Batch Invite')
    const email = activeDialog()!.querySelector<HTMLInputElement>('input[type="email"]')!
    email.value = 'new@org-a.example'
    email.dispatchEvent(new Event('input'))
    const dialog = wrapper!.findComponent(BatchInviteDialog)
    dialog.findComponent({ name: 'VSelect' }).vm.$emit('update:modelValue', A_ROLE)
    await settle()
    expect(dialog.findComponent({ name: 'VSelect' }).props('modelValue')).toBe(A_ROLE)

    await go(`/tenant/${ORG_B}/invites`)
    expect(activeDialog()).toBeNull()
    await clickButton('Batch Invite')
    expect(activeDialog()!.querySelector<HTMLInputElement>('input[type="email"]')!.value).toBe('')
    expect(dialog.findComponent({ name: 'VSelect' }).props('modelValue')).toBe('')
    // Under bun's runtime vue-tsc knows no props of a .vue component.
    const handed = dialog.props() as unknown as { tenantId: string; roles: Array<{ name: string }> }
    expect(handed.tenantId).toBe(ORG_B)
    expect(handed.roles.map((r) => r.name)).toEqual(['Org B operators'])
    expectNothingRaised()
  })

  it("a batch sent under org A that answers after the change stays out of org B's list, and its result never shows under org B", async () => {
    holdA = true
    await open(`/tenant/${ORG_A}/invites`)
    await clickButton('Batch Invite')
    const email = activeDialog()!.querySelector<HTMLInputElement>('input[type="email"]')!
    email.value = 'late@org-a.example'
    email.dispatchEvent(new Event('input'))
    await settle()
    await clickButton('Send All (1)', activeDialog()!)

    await go(`/tenant/${ORG_B}/invites`)
    releaseA()
    await settle()
    expect(pageText()).toContain('guest@org-b.example')
    expect(pageText()).not.toContain('late@org-a.example')
    await clickButton('Batch Invite')
    expect(activeDialog()!.textContent).not.toContain('invites created')
    expect(activeDialog()!.textContent).not.toContain('late@org-a.example')
    expectNothingRaised()
  })
})
