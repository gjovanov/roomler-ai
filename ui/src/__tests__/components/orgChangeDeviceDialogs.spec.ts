// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4332 — the five device dialogs show the org switch of the open device's
// org. The agents store holds each org switch (exec, SSH, external access) as
// one value, and each dialog fetched its switch only while that value was
// unset, whichever org it described. So once org A's switch was read (org A's
// Settings, or a dialog on one of org A's devices), a dialog opened on an
// org B device after a change of org skipped the fetch and showed org A's
// switch: on a reused page (a link, back, forward) and on a fresh one (the org
// switcher, then Devices) alike.
//
// AgentsSection mounts each dialog with `v-if` and opens it in the same tick,
// so each open here MOUNTS the dialog open, for org B, on a store that holds
// org A's switch. Every store is real and the API answers per org, so what a
// dialog shows is what its store holds for the org it was asked for.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import ExecPolicyDialog from '@/components/admin/ExecPolicyDialog.vue'
import DeviceConsoleDialog from '@/components/admin/DeviceConsoleDialog.vue'
import SshPolicyDialog from '@/components/admin/SshPolicyDialog.vue'
import RemoteConfigDialog from '@/components/admin/RemoteConfigDialog.vue'
import ExternalAccessDialog from '@/components/admin/ExternalAccessDialog.vue'
import { useAgentStore, type Agent } from '@/stores/agents'
import { useTenantStore } from '@/stores/tenant'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const vuetify = createVuetify({ components, directives })
const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })

/** A distinct 24-hex id. */
const hex = (n: number): string => n.toString(16).padStart(24, '0')

const ORG_A = hex(0xa000)
const ORG_B = hex(0xb000)
/** The org B device every dialog here is opened on. */
const DEVICE = hex(0xb004)

/** Each org's three switches (exec, SSH, external access), all on or all
 *  off. Each test sets both orgs, so they always differ. */
const switchOn: Record<string, boolean> = {}

/** GETs no answer below covers: none is expected. */
let unexpected: string[] = []

/** What the server answers for a GET, per org. */
function answer(path: string): unknown {
  const m = /^\/tenant\/([0-9a-f]{24})\/([^?]*)(?:\?.*)?$/.exec(path)
  const on = m ? switchOn[m[1]!] : undefined
  if (!m || on === undefined) {
    unexpected.push(path)
    return {}
  }
  const tag = m[1] === ORG_A ? 'Org A' : 'Org B'
  switch (m[2]) {
    case 'member/me':
      return { permissions: 0, is_owner: true }
    case 'member':
      return {
        items: [
          { id: hex(parseInt(m[1]!, 16) + 5), user_id: hex(parseInt(m[1]!, 16) + 1), display_name: `${tag} lead`, nickname: null, role_ids: [] },
        ],
      }
    case 'exec-settings':
      return { remote_exec_enabled: on, exec_require_system: false }
    case 'ssh-settings':
      return { remote_ssh_enabled: on }
    case 'external-access':
      return { enabled: on, devices: [] }
    case `agent/${DEVICE}/connect-code`:
      return { connect_code: null }
  }
  unexpected.push(path)
  return {}
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
  path.split('?')[0] === `/tenant/${tenant}/${route}`

/** Every GET sent so far, in order. */
const asked = (): string[] => mockApi.get.mock.calls.map((c) => c[0] as string)

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
let wrapper: VueWrapper | null = null

beforeEach(() => {
  pinia = createPinia()
  setActivePinia(pinia)
  vi.clearAllMocks()
  held = []
  unexpected = []
  raised = []
  mockApi.get.mockImplementation(async (path: string) => {
    const h = held.find((x) => !x.taken && x.match(path))
    if (!h) return answer(path) as never
    h.taken = true
    return new Promise((resolve) => {
      h.release = () => resolve(answer(path) as never)
    })
  })
  // The caller owns both orgs: every org switch is theirs to read.
  const t = useTenantStore()
  t.isOwner = true
  t.myPermissions = 0
  t.myPermissionsTenantId = ORG_A
})

afterEach(() => {
  wrapper?.unmount()
  wrapper = null
  document.body.innerHTML = ''
})

const globalOpts = () => ({
  plugins: [vuetify, i18n, pinia],
  config: { errorHandler: (err: unknown) => void raised.push(err) },
})

async function settle() {
  await flushPromises()
  await flushPromises()
}

/** Nothing raised while rendering, and no GET the server would not answer. */
function expectNothingRaised() {
  expect(raised.map(String)).toEqual([])
  expect(unexpected).toEqual([])
}

/** What the page says: a dialog's content is teleported to <body>. */
const shown = () => (document.body.textContent ?? '').replace(/\s+/g, ' ')

/** Org B's device, which every gate but its org's lets through: its own exec
 *  policy on, online, an agent that runs commands, SSH and outside sessions,
 *  and a requested config that turns exec and SSH on. */
function deviceOfB(): Agent {
  return {
    id: DEVICE,
    tenant_id: ORG_B,
    name: 'org-b-box',
    os: 'linux',
    agent_version: '0.4.235',
    is_online: true,
    presence: 'online',
    capabilities: {
      codecs: [],
      hw_encoders: [],
      has_input_permission: true,
      supports_clipboard: false,
      supports_file_transfer: false,
      max_simultaneous_sessions: 1,
      rpc: ['exec', 'ssh', 'external-access'],
    },
    exec_policy: {
      mode: 'on',
      can_originate: false,
      allowed_user_ids: [],
      allowed_role_ids: [],
      consent_mode: null,
      shells: [],
    },
    remote_config: {
      desired: { revision: 2, exec_enabled: true, ssh_enabled: true },
      state: 'applied',
    },
  } as unknown as Agent
}

type Store = ReturnType<typeof useAgentStore>

/** Each org switch: its GET after the org, its own fetcher, and the value the
 *  store holds for it (`null`: unknown). */
const SWITCHES = {
  exec: {
    route: 'exec-settings',
    fetch: (s: Store, t: string) => s.fetchOrgExecEnabled(t),
    value: (s: Store) => s.orgExecEnabled,
  },
  ssh: {
    route: 'ssh-settings',
    fetch: (s: Store, t: string) => s.fetchOrgSshEnabled(t),
    value: (s: Store) => s.orgSshEnabled,
  },
  external: {
    route: 'external-access',
    fetch: (s: Store, t: string) => s.fetchExternalAccess(t),
    value: (s: Store) => s.orgExternalAccessEnabled,
  },
} as const

interface DialogCase {
  name: string
  /** The org switches it shows. */
  switches: Array<keyof typeof SWITCHES>
  /** What it says while those switches are off. */
  off: string[]
  /** Mounts it open on org B's device for `tenantId`, as AgentsSection does. */
  open: (tenantId: string) => VueWrapper
}

const props = (tenantId: string) => ({ modelValue: true, tenantId, agent: deviceOfB() })

const DIALOGS: DialogCase[] = [
  {
    name: 'ExecPolicyDialog',
    switches: ['exec'],
    off: ['Remote execution is switched off for the whole organization'],
    open: (t) => mount(ExecPolicyDialog, { props: props(t), global: globalOpts(), attachTo: document.body }),
  },
  {
    name: 'DeviceConsoleDialog',
    switches: ['exec'],
    off: ['Remote execution is switched off for this organization'],
    open: (t) => mount(DeviceConsoleDialog, { props: props(t), global: globalOpts(), attachTo: document.body }),
  },
  {
    name: 'SshPolicyDialog',
    switches: ['ssh'],
    off: ['SSH is switched off for the whole organization'],
    open: (t) => mount(SshPolicyDialog, { props: props(t), global: globalOpts(), attachTo: document.body }),
  },
  {
    name: 'RemoteConfigDialog',
    switches: ['exec', 'ssh'],
    off: [
      'Remote execution is switched off for the whole organization',
      'SSH is switched off for the whole organization',
    ],
    open: (t) => mount(RemoteConfigDialog, { props: props(t), global: globalOpts(), attachTo: document.body }),
  },
  {
    name: 'ExternalAccessDialog',
    switches: ['external'],
    off: ['External access is switched off for the whole organization'],
    open: (t) => mount(ExternalAccessDialog, { props: props(t), global: globalOpts(), attachTo: document.body }),
  },
]

describe("The five device dialogs show the switch of their device's org (#4332)", () => {
  for (const d of DIALOGS) {
    const routes = d.switches.map((k) => SWITCHES[k].route)
    const values = (s: Store) => d.switches.map((k) => SWITCHES[k].value(s))
    /** Org A's switches, read through the real store, as org A's Settings or
     *  a dialog on one of org A's devices reads them. */
    async function readOrgA(s: Store) {
      for (const k of d.switches) await SWITCHES[k].fetch(s, ORG_A)
    }

    it(`${d.name} opened for org B while the store holds org A's switch (off): asks for org B's (on), and never shows org A's, not even while org B's answer is on its way`, async () => {
      switchOn[ORG_A] = false
      switchOn[ORG_B] = true
      const s = useAgentStore()
      await readOrgA(s)
      expect(values(s)).toEqual(d.switches.map(() => false))
      const since = asked().length
      const answersB = routes.map((r) => hold(getOf(ORG_B, r)))

      wrapper = d.open(ORG_B)
      // Org A's value is gone before the dialog renders: unknown until
      // org B's answer lands. Soft, so a failure here still reports what the
      // dialog then asks and shows.
      expect.soft(values(s)).toEqual(d.switches.map(() => null))
      await settle()
      for (const r of routes) expect.soft(asked().slice(since)).toContain(`/tenant/${ORG_B}/${r}`)
      for (const text of d.off) expect(shown()).not.toContain(text)

      for (const h of answersB) h.release()
      await settle()
      expect(values(s)).toEqual(d.switches.map(() => true))
      for (const text of d.off) expect(shown()).not.toContain(text)
      expectNothingRaised()
    })

    it(`${d.name} opened for org B while the store holds org A's switch (on): asks for org B's, and shows that org B's is off`, async () => {
      switchOn[ORG_A] = true
      switchOn[ORG_B] = false
      const s = useAgentStore()
      await readOrgA(s)
      expect(values(s)).toEqual(d.switches.map(() => true))
      const since = asked().length

      wrapper = d.open(ORG_B)
      await settle()
      // Soft, so a failure here still reports what the dialog then shows.
      for (const r of routes) expect.soft(asked().slice(since)).toContain(`/tenant/${ORG_B}/${r}`)
      expect.soft(values(s)).toEqual(d.switches.map(() => false))
      for (const text of d.off) expect(shown()).toContain(text)
      expectNothingRaised()
    })
  }
})

// A keep-working guard, not the bug: #4257 made each switch's fetch keep only
// its latest call's answer, so org A's answer, released after the dialog asked
// for org B's, never lands. These pass before #4332's fix as well.
describe("The five device dialogs: org A's late switch does not land (#4257, kept by #4332)", () => {
  for (const d of DIALOGS) {
    const routes = d.switches.map((k) => SWITCHES[k].route)
    const values = (s: Store) => d.switches.map((k) => SWITCHES[k].value(s))

    it(`${d.name}: org A's switch (off), answered after the dialog asked for org B's (on), does not land`, async () => {
      switchOn[ORG_A] = false
      switchOn[ORG_B] = true
      const s = useAgentStore()
      const lateA = routes.map((r) => hold(getOf(ORG_A, r)))
      const loadsA = d.switches.map((k) => SWITCHES[k].fetch(s, ORG_A))
      await settle()

      wrapper = d.open(ORG_B)
      await settle()
      for (const r of routes) expect(asked()).toContain(`/tenant/${ORG_B}/${r}`)
      for (const h of lateA) h.release()
      await Promise.all(loadsA)
      await settle()
      expect(values(s)).toEqual(d.switches.map(() => true))
      for (const text of d.off) expect(shown()).not.toContain(text)
      expectNothingRaised()
    })
  }
})
