// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4257 — the admin sections that loaded their org's data once, at mount or
// at setup. A routed section gets only a new `tenantId` prop when the org
// changes in place (back and forward, a link, the org switcher), so each kept
// showing the previous org's keys, nodes, MagicDNS, settings or roles, and an
// editor opened under the previous org stayed open, its save bound for the
// new one. Each section now follows its org: it asks for the new org's rows,
// closes what was opened under the previous org, and drops an answer for the
// previous org that lands late.
//
// Every store is real and the API answers per org, so what a section shows is
// what its store (or its own state) holds for the org it was asked for.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter } from 'vue-router'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import EnrollKeysSection from '@/components/admin/EnrollKeysSection.vue'
import OverlaySubnetRoutesSection from '@/components/admin/OverlaySubnetRoutesSection.vue'
import MagicDnsSection from '@/components/admin/MagicDnsSection.vue'
import SettingsSection from '@/components/admin/SettingsSection.vue'
import RolesSection from '@/components/admin/RolesSection.vue'
import MembersSection from '@/components/admin/MembersSection.vue'
import { useAgentStore } from '@/stores/agents'
import { useTenantStore } from '@/stores/tenant'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const vuetify = createVuetify({ components, directives })
const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })

/** A distinct 24-hex id. */
const hex = (n: number): string => n.toString(16).padStart(24, '0')

const ORG_A = hex(0xa000)
const ORG_B = hex(0xb000)

/** One org's rows, each named after the org. Org A's switches are all on and
 *  org B's all off, so every setting tells the two orgs apart. */
function makeOrg(tenant: string, tag: string, slug: string, on: boolean) {
  const base = parseInt(tenant, 16)
  return {
    tenant,
    tag,
    on,
    role: hex(base + 2),
    node: hex(base + 3),
    key: hex(base + 6),
    newRole: hex(base + 7),
    person: hex(base + 8),
    personName: `${tag} person`,
    roleName: `${tag} operators`,
    nodeName: `${tag} gateway`,
    keyLabel: `${tag} runners`,
    secret: `${tag} secret`,
    domain: `${slug}.example`,
  }
}

const ORGS: Record<string, ReturnType<typeof makeOrg>> = {
  [ORG_A]: makeOrg(ORG_A, 'Org A', 'org-a', true),
  [ORG_B]: makeOrg(ORG_B, 'Org B', 'org-b', false),
}
const A = ORGS[ORG_A]!
const B = ORGS[ORG_B]!

/** The org settings the Settings page reads, as the agents store holds them. */
function settingsOf(o: typeof A) {
  return {
    exec: o.on,
    requireSystem: o.on,
    ssh: o.on,
    keepBusyDenied: o.on,
    keepBusyPattern: o.on ? 'heart' : null,
    rcPolicy: o.on
      ? { clipboard: 'off', files: 'off', audio: 'off' }
      : { clipboard: 'on', files: 'ask', audio: 'on' },
    ephemeralKeys: o.on,
    externalAccess: o.on,
    maxSessionSecs: o.on ? 3600 : null,
    idleSecs: o.on ? 600 : null,
  }
}

function heldSettings() {
  const s = useAgentStore()
  return {
    exec: s.orgExecEnabled,
    requireSystem: s.orgExecRequireSystem,
    ssh: s.orgSshEnabled,
    keepBusyDenied: s.orgKeepBusyDenied,
    keepBusyPattern: s.orgKeepBusyDefaultPattern,
    rcPolicy: s.orgRcSessionPolicy,
    ephemeralKeys: s.orgEphemeralKeysEnabled,
    externalAccess: s.orgExternalAccessEnabled,
    maxSessionSecs: s.orgRcMaxSessionSecs,
    idleSecs: s.orgRcIdleSecs,
  }
}

/** The seven routes the Settings page reads, one per setting. */
const SETTINGS_ROUTES = [
  'rc-session-limits',
  'exec-settings',
  'ssh-settings',
  'keep-busy-settings',
  'rc-session-policy',
  'ephemeral-key-settings',
  'external-access',
]

/** Orgs whose ephemeral keys are switched off: by default both are on, so the
 *  Devices page's keys card shows for both. */
let keysOff = new Set<string>()

/** Requests no answer below covers: none is expected. */
let unexpected: string[] = []

/** What the server answers, per org. */
function answer(method: string, path: string, body?: unknown): unknown {
  const m = /^\/tenant\/([0-9a-f]{24})\/([^?]*)(?:\?(.*))?$/.exec(path)
  const org = m ? ORGS[m[1]!] : undefined
  if (!m || !org) {
    unexpected.push(`${method} ${path}`)
    return { items: [] }
  }
  const tenant = m[1]!
  const route = `${method} ${m[2]}`
  const s = settingsOf(org)
  switch (route) {
    case 'GET member/me':
      return { permissions: 0, is_owner: true }
    case 'GET member': {
      // Org A's people fill three pages, so a test can leave its first one.
      const q = new URLSearchParams(m[3] ?? '')
      const total = org.on ? 60 : 1
      return {
        items: [
          {
            id: org.person,
            user_id: org.person,
            display_name: org.personName,
            email: `person@${org.domain}`,
            role_ids: [org.role],
            joined_at: '2026-11-01T00:00:00Z',
          },
        ],
        total,
        page: Number(q.get('page') ?? 1),
        per_page: 25,
        total_pages: Math.ceil(total / 25),
      }
    }
    case 'GET agent':
      return { items: [], total: 0, page: 1, per_page: 100, total_pages: 0 }
    case 'GET tunnel-client':
      return { items: [], total: 0 }
    case 'GET keep-busy-history':
      return []
    case 'GET ephemeral-key-settings':
      return { ephemeral_keys_enabled: !keysOff.has(tenant) }
    case 'GET agent/enroll-key':
      return {
        items: [
          {
            id: org.key,
            jti: `${org.tag} jti`,
            label: org.keyLabel,
            created_by: org.tag,
            max_uses: 100,
            uses: 0,
            expires_at: '2099-01-01T00:00:00Z',
            created_at: '2026-11-01T00:00:00Z',
          },
        ],
      }
    case 'POST agent/enroll-key':
      return {
        key: org.secret,
        id: org.key,
        jti: `${org.tag} jti`,
        label: org.keyLabel,
        max_uses: 100,
        expires_at: '2099-01-01T00:00:00Z',
      }
    case 'GET overlay-node':
      return {
        items: [
          {
            id: org.node,
            name: org.nodeName,
            overlay_ip: '',
            kind: 'agent',
            advertised_routes: [],
            approved_routes: [],
            is_exit_node: false,
            can_be_exit_node: true,
            online: true,
            will_rejoin: true,
            last_seen_at: '2026-11-21T00:00:00Z',
          },
        ],
      }
    case 'GET magic-dns':
      return { magic_dns_domain: org.domain, magic_dns_nameservers: [] }
    case 'PUT magic-dns':
      // The server answers what it stored.
      return body
    case 'GET rc-session-limits':
      return { max_secs: s.maxSessionSecs, idle_secs: s.idleSecs }
    case 'GET exec-settings':
      return { remote_exec_enabled: s.exec, exec_require_system: s.requireSystem }
    case 'GET ssh-settings':
      return { remote_ssh_enabled: s.ssh }
    case 'GET keep-busy-settings':
      return { keep_busy_denied: s.keepBusyDenied, keep_busy_default_pattern: s.keepBusyPattern }
    case 'GET rc-session-policy':
      return s.rcPolicy
    case 'GET external-access':
      return { enabled: s.externalAccess, devices: [] }
    case 'GET role':
      return [
        {
          id: org.role,
          tenant_id: tenant,
          name: org.roleName,
          position: 10,
          permissions: 0,
          is_default: false,
          is_managed: false,
          is_mentionable: false,
        },
      ]
    case 'POST role':
      return {
        id: org.newRole,
        tenant_id: tenant,
        name: (body as { name: string }).name,
        position: 20,
        permissions: 0,
        is_default: false,
        is_managed: false,
        is_mentionable: false,
      }
  }
  unexpected.push(`${method} ${path}`)
  return { items: [] }
}

/** A request whose answer the test hands over later. */
interface Held {
  method: string
  match: (path: string) => boolean
  taken: boolean
  release: () => void
}
let held: Held[] = []

function hold(method: 'GET' | 'POST' | 'PUT', match: (path: string) => boolean): Held {
  const h: Held = {
    method,
    match,
    taken: false,
    release: () => {
      throw new Error(`the held ${method} was never sent`)
    },
  }
  held.push(h)
  return h
}

/** A request of `route` (the path after the tenant) for `tenant`. */
const pathOf = (tenant: string, route: string) => (path: string) =>
  path === `/tenant/${tenant}/${route}` || path.startsWith(`/tenant/${tenant}/${route}?`)

/** Every GET sent so far, in order. */
const asked = (): string[] => mockApi.get.mock.calls.map((c) => c[0] as string)

/** The requests that answer through `answer`, or through a held promise. */
function respond(method: string) {
  return async (path: string, body?: unknown) => {
    const h = held.find((x) => !x.taken && x.method === method && x.match(path))
    if (!h) return answer(method, path, body) as never
    h.taken = true
    return new Promise((resolve) => {
      h.release = () => resolve(answer(method, path, body) as never)
    }) as never
  }
}

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
let raised: unknown[]

beforeEach(() => {
  pinia = createPinia()
  setActivePinia(pinia)
  vi.clearAllMocks()
  held = []
  unexpected = []
  raised = []
  keysOff = new Set()
  mockApi.get.mockImplementation(respond('GET'))
  mockApi.post.mockImplementation(respond('POST'))
  mockApi.put.mockImplementation(respond('PUT'))
  // The caller owns both orgs: every org setting is theirs to read and change.
  const t = useTenantStore()
  t.isOwner = true
  t.myPermissions = 0
  t.myPermissionsTenantId = ORG_A
})

afterEach(() => {
  document.body.innerHTML = ''
})

function globalOpts(extra: unknown[] = []) {
  return {
    plugins: [vuetify, i18n, pinia, ...extra] as never[],
    config: { errorHandler: (err: unknown) => void raised.push(err) },
  }
}

async function settle() {
  await flushPromises()
  await flushPromises()
  await flushPromises()
}

/** The page's org changes in place: a routed section gets a new prop. Under
 *  bun's runtime a `.vue` component's props are not typed (vue-tsc in
 *  "Frontend checks"), hence the cast. */
async function changeOrg(w: VueWrapper, tenantId: string) {
  await w.setProps({ tenantId } as unknown as Parameters<typeof w.setProps>[0])
  await settle()
}

/** Nothing raised while rendering or handling a change, and no request the
 *  server would not answer. */
function expectNothingRaised() {
  expect(raised.map(String)).toEqual([])
  expect(unexpected).toEqual([])
}

/** The dialog on screen, if any (a closed one can stay in the DOM). */
const openDialog = (): HTMLElement | null =>
  document.querySelector<HTMLElement>('.v-dialog.v-overlay--active')

/** A button by its text, inside `root`. */
function buttonIn(root: ParentNode, text: string): HTMLElement {
  const b = Array.from(root.querySelectorAll<HTMLElement>('button')).find(
    (x) => x.textContent?.trim() === text,
  )
  if (!b) throw new Error(`no "${text}" button`)
  return b
}

async function click(el: HTMLElement) {
  el.click()
  await settle()
}

async function typeInto(input: HTMLInputElement, value: string) {
  input.value = value
  input.dispatchEvent(new Event('input'))
  await settle()
}

describe("EnrollKeysSection: the org's ephemeral keys (#4257)", () => {
  /** Whether any text box on the page holds `text` (the minted key is shown
   *  in a read-only text area, as its value). */
  const shownInABox = (text: string) =>
    Array.from(document.querySelectorAll<HTMLTextAreaElement>('textarea')).some((t) =>
      t.value.includes(text),
    )

  async function mintFor(w: VueWrapper) {
    await click(buttonIn(w.element, 'Mint key'))
    const dialog = openDialog()
    expect(dialog).not.toBeNull()
    await click(buttonIn(dialog!, 'Mint'))
  }

  it("mounted for org A, then org B: asks for org B's switch and keys, and lists org B's keys", async () => {
    const w = mount(EnrollKeysSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    expect(w.text()).toContain(A.keyLabel)
    const since = asked().length

    await changeOrg(w, ORG_B)
    expect(asked().slice(since)).toContain(`/tenant/${ORG_B}/ephemeral-key-settings`)
    expect(asked().slice(since)).toContain(`/tenant/${ORG_B}/agent/enroll-key`)
    expect(w.text()).toContain(B.keyLabel)
    expect(w.text()).not.toContain(A.keyLabel)
    expectNothingRaised()
    w.unmount()
  })

  it("org A's keys answering after the change do not land", async () => {
    const lateA = hold('GET', pathOf(ORG_A, 'agent/enroll-key'))
    const w = mount(EnrollKeysSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()

    await changeOrg(w, ORG_B)
    lateA.release()
    await settle()
    expect(w.text()).toContain(B.keyLabel)
    expect(w.text()).not.toContain(A.keyLabel)
    expectNothingRaised()
    w.unmount()
  })

  it('the mint dialog opened under org A closes on the change, and its minted secret goes with it', async () => {
    const w = mount(EnrollKeysSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    await mintFor(w)
    expect(shownInABox(A.secret)).toBe(true)

    await changeOrg(w, ORG_B)
    expect(openDialog()).toBeNull()
    expect(shownInABox(A.secret)).toBe(false)
    expectNothingRaised()
    w.unmount()
  })

  it("a key minted for org A that answers after the change never shows its secret", async () => {
    const lateMint = hold('POST', pathOf(ORG_A, 'agent/enroll-key'))
    const w = mount(EnrollKeysSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    await mintFor(w)

    await changeOrg(w, ORG_B)
    lateMint.release()
    await settle()
    expect(openDialog()).toBeNull()
    expect(shownInABox(A.secret)).toBe(false)
    expectNothingRaised()
    w.unmount()
  })
})

describe("OverlaySubnetRoutesSection: the org's overlay nodes (#4257)", () => {
  function makeRouter() {
    return createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/:rest(.*)*', component: { render: () => null } }],
    })
  }

  it("mounted for org A, then org B: asks for org B's nodes and lists them, and org A's late answer does not land", async () => {
    const router = makeRouter()
    await router.push('/')
    await router.isReady()
    const lateA = hold('GET', pathOf(ORG_A, 'overlay-node'))
    const w = mount(OverlaySubnetRoutesSection, {
      props: { tenantId: ORG_A },
      global: globalOpts([router]),
      attachTo: document.body,
    })
    await settle()
    const since = asked().length

    await changeOrg(w, ORG_B)
    expect(asked().slice(since)).toContain(`/tenant/${ORG_B}/overlay-node`)
    expect(w.text()).toContain(B.nodeName)
    lateA.release()
    await settle()
    expect(w.text()).toContain(B.nodeName)
    expect(w.text()).not.toContain(A.nodeName)
    expectNothingRaised()
    w.unmount()
  })
})

describe("MagicDnsSection: the org's MagicDNS settings (#4257)", () => {
  const domainInput = (w: VueWrapper) => w.findAll('input')[0]!.element as HTMLInputElement

  it("mounted for org A, then org B: asks for org B's settings and shows them", async () => {
    const w = mount(MagicDnsSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    expect(domainInput(w).value).toBe(A.domain)
    const since = asked().length

    await changeOrg(w, ORG_B)
    expect(asked().slice(since)).toContain(`/tenant/${ORG_B}/magic-dns`)
    expect(domainInput(w).value).toBe(B.domain)
    expectNothingRaised()
    w.unmount()
  })

  it("org A's settings answering after the change do not fill the form", async () => {
    const lateA = hold('GET', pathOf(ORG_A, 'magic-dns'))
    const w = mount(MagicDnsSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()

    await changeOrg(w, ORG_B)
    lateA.release()
    await settle()
    expect(domainInput(w).value).toBe(B.domain)
    expectNothingRaised()
    w.unmount()
  })

  it("an edit made under org A is not kept under org B, so it is never saved there", async () => {
    const w = mount(MagicDnsSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    await typeInto(domainInput(w), 'edited-under-a.example')
    const loadB = hold('GET', pathOf(ORG_B, 'magic-dns'))

    await changeOrg(w, ORG_B)
    expect(domainInput(w).value).toBe('')
    loadB.release()
    await settle()
    expect(domainInput(w).value).toBe(B.domain)
    expectNothingRaised()
    w.unmount()
  })

  it("a save for org A answering after the change leaves org B's settings in the form", async () => {
    const w = mount(MagicDnsSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    const lateSave = hold('PUT', pathOf(ORG_A, 'magic-dns'))
    await click(buttonIn(w.element, 'Save'))

    await changeOrg(w, ORG_B)
    lateSave.release()
    await settle()
    expect(domainInput(w).value).toBe(B.domain)
    expectNothingRaised()
    w.unmount()
  })
})

describe("SettingsSection: the org's settings (#4257)", () => {
  beforeEach(() => {
    // Org B's keys switch is off, as its other switches are.
    keysOff.add(ORG_B)
  })

  it("mounted for org A, then org B: asks for org B's seven settings, and holds them", async () => {
    const w = mount(SettingsSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    expect(heldSettings()).toEqual(settingsOf(A))
    const since = asked().length

    await changeOrg(w, ORG_B)
    for (const route of SETTINGS_ROUTES) {
      expect(asked().slice(since)).toContain(`/tenant/${ORG_B}/${route}`)
    }
    expect(heldSettings()).toEqual(settingsOf(B))
    expectNothingRaised()
    w.unmount()
  })

  it("org A's settings answering after the change do not land", async () => {
    const lateA = SETTINGS_ROUTES.map((route) => hold('GET', pathOf(ORG_A, route)))
    const w = mount(SettingsSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()

    await changeOrg(w, ORG_B)
    for (const h of lateA) h.release()
    await settle()
    expect(heldSettings()).toEqual(settingsOf(B))
    expectNothingRaised()
    w.unmount()
  })
})

describe("RolesSection: the org's roles (#4257)", () => {
  /** The icon button `icon` on the role row named `name`. */
  function rowButton(w: VueWrapper, name: string, icon: string): HTMLElement {
    const row = w.findAll('tr').find((r) => r.text().includes(name))
    if (!row) throw new Error(`no row for "${name}"`)
    const b = row.findAll('button').find((x) => x.find(`.${icon}`).exists())
    if (!b) throw new Error(`no ${icon} button on "${name}"`)
    return b.element as HTMLElement
  }

  it("mounted for org A, then org B: asks for org B's roles and lists them", async () => {
    const w = mount(RolesSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    expect(w.text()).toContain(A.roleName)
    const since = asked().length

    await changeOrg(w, ORG_B)
    expect(asked().slice(since)).toEqual([`/tenant/${ORG_B}/role`])
    expect(w.text()).toContain(B.roleName)
    expect(w.text()).not.toContain(A.roleName)
    expectNothingRaised()
    w.unmount()
  })

  it('the role editor opened on an org A role closes on the change', async () => {
    const w = mount(RolesSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    await click(rowButton(w, A.roleName, 'mdi-pencil'))
    expect(openDialog()).not.toBeNull()

    await changeOrg(w, ORG_B)
    expect(openDialog()).toBeNull()
    expectNothingRaised()
    w.unmount()
  })

  it('the delete prompt opened on an org A role closes on the change', async () => {
    const w = mount(RolesSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    await click(rowButton(w, A.roleName, 'mdi-delete'))
    expect(openDialog()).not.toBeNull()

    await changeOrg(w, ORG_B)
    expect(openDialog()).toBeNull()
    expectNothingRaised()
    w.unmount()
  })

  it("a role created for org A that answers after the change stays out of org B's list", async () => {
    const w = mount(RolesSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })
    await settle()
    await click(buttonIn(w.element, 'New Role'))
    const dialog = openDialog()
    expect(dialog).not.toBeNull()
    await typeInto(dialog!.querySelector('input')!, 'Org A auditors')
    const lateCreate = hold('POST', pathOf(ORG_A, 'role'))
    await click(buttonIn(dialog!, 'Create'))

    await changeOrg(w, ORG_B)
    lateCreate.release()
    await settle()
    expect(w.text()).toContain(B.roleName)
    expect(w.text()).not.toContain('Org A auditors')
    expectNothingRaised()
    w.unmount()
  })
})

describe("MembersSection: the org's people and their roles (#4257)", () => {
  const mountMembers = () =>
    mount(MembersSection, { props: { tenantId: ORG_A }, global: globalOpts(), attachTo: document.body })

  /** A button by its text, on the grid row that names `who`. */
  function rowButton(w: VueWrapper, who: string, text: string): HTMLElement {
    const row = Array.from((w.element as HTMLElement).querySelectorAll<HTMLElement>('tbody tr')).find((r) =>
      r.textContent?.includes(who),
    )
    if (!row) throw new Error(`no row for ${who}`)
    return buttonIn(row, text)
  }

  /** The search box waits 300 ms after a keystroke, a clear included. */
  const debounce = () => new Promise((r) => setTimeout(r, 350))

  it("mounted for org A, then org B: asks for org B's people and roles, and lists org B's people with org B's roles", async () => {
    const w = mountMembers()
    await settle()
    expect(w.text()).toContain(A.personName)
    expect(w.text()).toContain(A.roleName)
    const since = asked().length

    await changeOrg(w, ORG_B)
    const after = asked().slice(since)
    expect(after.filter(pathOf(ORG_B, 'member'))).toHaveLength(1)
    expect(after).toContain(`/tenant/${ORG_B}/role`)
    expect(w.text()).toContain(B.personName)
    expect(w.text()).toContain(B.roleName)
    expect(w.text()).not.toContain(A.personName)
    expect(w.text()).not.toContain(A.roleName)
    expectNothingRaised()
    w.unmount()
  })

  it("org A's people and roles answering after the change do not land", async () => {
    const latePeople = hold('GET', pathOf(ORG_A, 'member'))
    const lateRoles = hold('GET', pathOf(ORG_A, 'role'))
    const w = mountMembers()
    await settle()

    await changeOrg(w, ORG_B)
    latePeople.release()
    lateRoles.release()
    await settle()
    expect(w.text()).toContain(B.personName)
    expect(w.text()).toContain(B.roleName)
    expect(w.text()).not.toContain(A.personName)
    expect(w.text()).not.toContain(A.roleName)
    expectNothingRaised()
    w.unmount()
  })

  it('a search and a page chosen under org A do not carry over: org B is asked for its first page, unfiltered', async () => {
    const w = mountMembers()
    await settle()
    const box = w.get('input[placeholder="Search name or email"]').element as HTMLInputElement
    await typeInto(box, 'person')
    await debounce()
    await settle()
    await click(w.get('button[aria-label="Next page"]').element as HTMLElement)
    const forA = asked().filter(pathOf(ORG_A, 'member'))
    expect(forA.at(-1)).toContain('page=2&')
    expect(forA.at(-1)).toContain('q=person')
    const since = asked().length

    await changeOrg(w, ORG_B)
    await debounce()
    await settle()
    const forB = asked().slice(since).filter(pathOf(ORG_B, 'member'))
    expect(forB.length).toBeGreaterThan(0)
    for (const path of forB) {
      expect(path).toContain('page=1&')
      expect(path).not.toContain('q=')
    }
    expect(box.value).toBe('')
    expectNothingRaised()
    w.unmount()
  })

  it('the add dialog opened under org A closes on the change', async () => {
    const w = mountMembers()
    await settle()
    await click(buttonIn(w.element, 'Add member'))
    expect(openDialog()?.textContent).toContain('Add member by email')

    await changeOrg(w, ORG_B)
    expect(openDialog()).toBeNull()
    expectNothingRaised()
    w.unmount()
  })

  it('the remove prompt opened on an org A person closes on the change, and nothing is removed', async () => {
    const w = mountMembers()
    await settle()
    await click(rowButton(w, A.personName, 'Remove'))
    expect(openDialog()?.textContent).toContain(`Remove ${A.personName}`)

    await changeOrg(w, ORG_B)
    expect(openDialog()).toBeNull()
    expect(mockApi.delete).not.toHaveBeenCalled()
    expectNothingRaised()
    w.unmount()
  })

  it("the roles dialog opened on an org A person closes on the change", async () => {
    const w = mountMembers()
    await settle()
    await click(rowButton(w, A.personName, 'Roles'))
    expect(openDialog()?.textContent).toContain(`Roles — ${A.personName}`)

    await changeOrg(w, ORG_B)
    expect(openDialog()).toBeNull()
    expectNothingRaised()
    w.unmount()
  })
})
