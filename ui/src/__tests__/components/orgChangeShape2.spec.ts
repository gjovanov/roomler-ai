// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4257 — Shape 2: a fetch skipped while a store's list has rows, whichever
// org they belong to. Each component here asked for the org's rows only while
// its store's list was empty, and a list that holds another org's rows is not
// empty: mounted or opened for org B after org A's page had loaded them, it
// named, listed or offered org A's rows. A component whose org can change in
// place (a prop) must follow it, and a late answer for org A must not land.
// The audit logs, last, are Shape 1 as well: each loaded its log once, on
// mount.
//
// Every store is real and the API answers per org, so what a component shows
// is what its store holds for the org it was asked for.
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

import RoomMemberList from '@/components/rooms/RoomMemberList.vue'
import RoleAssignDialog from '@/components/rooms/RoleAssignDialog.vue'
import KeepBusyHistorySection from '@/components/admin/KeepBusyHistorySection.vue'
import ExecAuditSection from '@/components/admin/ExecAuditSection.vue'
import SshAuditSection from '@/components/admin/SshAuditSection.vue'
import SshActivitySection from '@/components/admin/SshActivitySection.vue'
import ExternalAuditSection from '@/components/admin/ExternalAuditSection.vue'
import ExecPolicyDialog from '@/components/admin/ExecPolicyDialog.vue'
import SshPolicyDialog from '@/components/admin/SshPolicyDialog.vue'
import { useRoleStore } from '@/stores/role'
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

/** One org's rows, each named after the org; `historyPage` keep-busy rows. */
function makeOrg(tenant: string, tag: string, historyPage: number) {
  const base = parseInt(tenant, 16)
  return {
    tag,
    historyPage,
    user: hex(base + 1),
    role: hex(base + 2),
    agent: hex(base + 4),
    member: hex(base + 5),
    lead: `${tag} lead`,
    roleName: `${tag} operators`,
    box: `${tag} box`,
  }
}

// Org A's history fills a page, so it offers "Load more".
const ORGS: Record<string, ReturnType<typeof makeOrg>> = {
  [ORG_A]: makeOrg(ORG_A, 'Org A', 50),
  [ORG_B]: makeOrg(ORG_B, 'Org B', 2),
}
const A = ORGS[ORG_A]!
const B = ORGS[ORG_B]!

function historyEntry(tenant: string, i: number | string) {
  const org = ORGS[tenant]!
  return {
    id: `${org.tag} entry ${i}`,
    tenant_id: tenant,
    at: `2026-11-01T12:00:${String(typeof i === 'number' ? 59 - i : 0).padStart(2, '0')}.000Z`,
    kind: 'on',
    source: 'device',
    agent_id: org.agent,
    agent_name: org.box,
    actor_name: org.lead,
    pattern: 'heart',
  }
}

/** GETs no answer below covers: none is expected. */
let unexpected: string[] = []

/** What the server answers for a GET, per org. */
function answer(path: string): unknown {
  const m = /^\/tenant\/([0-9a-f]{24})\/([^?]*)(?:\?(.*))?$/.exec(path)
  const org = m ? ORGS[m[1]!] : undefined
  if (!m || !org) {
    unexpected.push(path)
    return { items: [] }
  }
  const tenant = m[1]!
  const q = new URLSearchParams(m[3] ?? '')
  switch (m[2]) {
    case 'role':
      return [
        {
          id: org.role,
          tenant_id: tenant,
          name: org.roleName,
          position: 1,
          permissions: 0,
          is_default: false,
          is_managed: false,
          is_mentionable: false,
        },
      ]
    case 'member':
      return {
        items: [
          { id: org.member, user_id: org.user, display_name: org.lead, nickname: null, role_ids: [org.role] },
        ],
      }
    case 'member/me':
      return { permissions: 0, is_owner: true }
    case 'agent':
      return {
        items: [{ id: org.agent, name: org.box.toLowerCase().replace(/ /g, '-'), display_name: org.box }],
        total: 1,
        page: 1,
        per_page: 100,
        total_pages: 1,
      }
    case 'keep-busy-history':
      if (q.get('before')) return [historyEntry(tenant, 'more')]
      return Array.from({ length: org.historyPage }, (_, i) => historyEntry(tenant, i))
    case 'exec-audit':
      return {
        items: [
          {
            tenant_id: tenant,
            agent_id: org.agent,
            user_id: org.user,
            request_id: `${org.tag} request`,
            source: 'cli',
            shell: 'sh',
            command: `${org.tag} uptime`,
            at: '2026-11-01T12:00:00.000Z',
            exit_code: 0,
          },
        ],
        total: 1,
      }
    case 'ssh-audit':
      return {
        items: [
          {
            id: `${org.tag} grant`,
            tenant_id: tenant,
            agent_id: org.agent,
            user_id: org.user,
            source: `${org.tag} cli`,
            caller: org.user,
            account_mode: 'daemon',
            at: '2026-11-01T12:00:00.000Z',
          },
        ],
        total: 1,
      }
    case 'ssh-activity':
      return {
        items: [
          {
            id: `${org.tag} activity`,
            agent_id: org.agent,
            caller: `ssh:${org.lead}@node:22`,
            kind: 'exec',
            detail: `${org.tag} uptime`,
            allowed: true,
            at: '2026-11-01T12:00:00.000Z',
          },
        ],
        total: 1,
      }
    case 'external-rc-audit':
      return {
        items: [
          {
            id: `${org.tag} external`,
            action: 'login',
            agent_id: org.agent,
            user_id: org.user,
            actor: `${org.tag} visitor`,
            at: '2026-11-01T12:00:00.000Z',
          },
        ],
        total: 1,
      }
    case 'external-principals':
      return {
        items: [
          {
            agent_id: org.agent,
            user_id: org.user,
            name: `${org.tag} outsider`,
            first_session_at: '2026-11-01T12:00:00.000Z',
            last_session_at: '2026-11-01T12:00:00.000Z',
          },
        ],
        total: 1,
      }
    case 'exec-settings':
      return { remote_exec_enabled: true }
    case 'ssh-settings':
      return { remote_ssh_enabled: true }
  }
  unexpected.push(path)
  return { items: [] }
}

/** A GET whose answer the test hands over later. */
interface Held {
  match: (path: string) => boolean
  taken: boolean
  /** The server refuses it, with this message, instead of answering. */
  fail?: string
  release: () => void
}
let held: Held[] = []

function hold(match: (path: string) => boolean, fail?: string): Held {
  const h: Held = {
    match,
    taken: false,
    fail,
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
  // A test that holds a PUT gives it an implementation: clearAllMocks keeps it.
  mockApi.put.mockReset()
  held = []
  unexpected = []
  raised = []
  mockApi.get.mockImplementation(async (path: string) => {
    const h = held.find((x) => !x.taken && x.match(path))
    if (!h) return answer(path) as never
    h.taken = true
    return new Promise((resolve, reject) => {
      h.release = () => (h.fail ? reject(new Error(h.fail)) : resolve(answer(path) as never))
    })
  })
  // The caller owns both orgs: every org setting is theirs to read.
  const t = useTenantStore()
  t.isOwner = true
  t.myPermissions = 0
  t.myPermissionsTenantId = ORG_A
})

afterEach(() => {
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

/** Nothing raised while rendering or handling a change, and no GET the
 *  server would not answer. */
function expectNothingRaised() {
  expect(raised.map(String)).toEqual([])
  expect(unexpected).toEqual([])
}

describe("RoomMemberList and RoleAssignDialog: the org's roles (#4257)", () => {
  // No page mounts RoomMemberList today, and it is RoleAssignDialog's only
  // parent: each is tested on its own.
  const memberOf = (o: typeof A) => ({
    id: o.member,
    user_id: o.user,
    display_name: o.lead,
    role_ids: [o.role],
  })
  // Under bun's runtime a `.vue` component's props are not typed (vue-tsc in
  // "Frontend checks"), so the dialog's state is read through a cast.
  const dialogOpen = (w: VueWrapper) =>
    (w.findComponent(RoleAssignDialog).props() as unknown as { modelValue: boolean }).modelValue

  it("RoomMemberList mounted for org B while the role store holds org A's roles: asks for org B's, and names them", async () => {
    await useRoleStore().fetchRoles(ORG_A)
    const since = asked().length
    const w = mount(RoomMemberList, {
      props: { tenantId: ORG_B, members: [memberOf(B)], canManageRoles: true },
      global: globalOpts(),
      attachTo: document.body,
    })
    await settle()
    expect(asked().slice(since)).toEqual([`/tenant/${ORG_B}/role`])
    expect(w.text()).toContain(B.roleName)
    expectNothingRaised()
    w.unmount()
  })

  it('RoomMemberList follows its org: asks for org B\'s roles, and closes a role dialog opened under org A', async () => {
    const w = mount(RoomMemberList, {
      props: { tenantId: ORG_A, members: [memberOf(A)], canManageRoles: true },
      global: globalOpts(),
      attachTo: document.body,
    })
    await settle()
    expect(w.text()).toContain(A.roleName)
    const assign = w.findAll('button').find((b) => b.find('.mdi-shield-plus').exists())
    await assign!.trigger('click')
    await settle()
    expect(dialogOpen(w)).toBe(true)

    const since = asked().length
    await w.setProps({ tenantId: ORG_B, members: [memberOf(B)] } as unknown as Parameters<
      typeof w.setProps
    >[0])
    await settle()
    expect(asked().slice(since)).toEqual([`/tenant/${ORG_B}/role`])
    expect(w.text()).toContain(B.roleName)
    expect(dialogOpen(w)).toBe(false)
    expectNothingRaised()
    w.unmount()
  })

  it("RoleAssignDialog opened for org B while the role store holds org A's roles: asks for org B's, and offers them", async () => {
    await useRoleStore().fetchRoles(ORG_A)
    const since = asked().length
    const w = mount(RoleAssignDialog, {
      props: { modelValue: false, tenantId: ORG_B, userId: B.user, memberName: B.lead },
      global: globalOpts(),
      attachTo: document.body,
    })
    await w.setProps({ modelValue: true } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    expect(asked().slice(since)).toEqual([`/tenant/${ORG_B}/role`])
    const offered = (w.findComponent(components.VSelect).props('items') as { id: string }[]).map(
      (r) => r.id,
    )
    expect(offered).toEqual([B.role])
    expectNothingRaised()
    w.unmount()
  })
})

describe('KeepBusyHistorySection: the org changes (#4257)', () => {
  const rows = () => [...document.querySelectorAll('[data-testid="keep-busy-history-row"]')]
  const rowText = () => rows().map((r) => r.textContent ?? '').join('\n')
  const mountHistory = (tenantId: string) =>
    mount(KeepBusyHistorySection, {
      props: { tenantId },
      global: globalOpts(),
      attachTo: document.body,
    })

  it("asks for org B's history and devices, and lists org B's rows", async () => {
    const w = mountHistory(ORG_A)
    await settle()
    expect(rows()).toHaveLength(50)

    const since = asked().length
    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    const after = asked().slice(since)
    expect(after).toEqual(
      expect.arrayContaining([
        `/tenant/${ORG_B}/keep-busy-history?limit=50`,
        `/tenant/${ORG_B}/agent?per_page=100`,
      ]),
    )
    expect(after.filter((p) => p.includes(ORG_A))).toEqual([])
    expect(rows()).toHaveLength(2)
    expect(rowText()).toContain(B.lead)
    expect(rowText()).not.toContain(A.lead)
    expectNothingRaised()
    w.unmount()
  })

  it('resets a device filter chosen under org A', async () => {
    const w = mountHistory(ORG_A)
    await settle()
    const select = () => w.findComponent({ name: 'VSelect' })
    select().vm.$emit('update:modelValue', A.agent)
    await settle()
    expect(asked().at(-1)).toBe(`/tenant/${ORG_A}/keep-busy-history?agent_id=${A.agent}&limit=50`)

    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    expect(select().props('modelValue')).toBe('')
    expect(asked().filter((p) => p.startsWith(`/tenant/${ORG_B}/keep-busy-history`))).toEqual([
      `/tenant/${ORG_B}/keep-busy-history?limit=50`,
    ])
    expectNothingRaised()
    w.unmount()
  })

  it("drops org A's history when it answers after the change", async () => {
    const late = hold(getOf(ORG_A, 'keep-busy-history?'))
    const w = mountHistory(ORG_A)
    await settle()
    expect(late.taken).toBe(true)

    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    late.release()
    await settle()
    expect(rows()).toHaveLength(2)
    expect(rowText()).not.toContain(A.lead)
    expectNothingRaised()
    w.unmount()
  })

  it("drops org A's next page when it answers after the change", async () => {
    const w = mountHistory(ORG_A)
    await settle()
    expect(rows()).toHaveLength(50)
    const late = hold((p) => getOf(ORG_A, 'keep-busy-history?')(p) && p.includes('before='))
    await w.find('[data-testid="keep-busy-history-more"]').trigger('click')
    await settle()
    expect(late.taken).toBe(true)

    await w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])
    await settle()
    late.release()
    await settle()
    expect(rows()).toHaveLength(2)
    expect(rowText()).not.toContain(A.lead)
    expectNothingRaised()
    w.unmount()
  })

  it("mounted for org B while the device store holds org A's: asks for org B's devices, and offers them", async () => {
    await useAgentStore().fetchAgents(ORG_A)
    const since = asked().length
    const w = mountHistory(ORG_B)
    await settle()
    expect(asked().slice(since)).toContain(`/tenant/${ORG_B}/agent?per_page=100`)
    const offered = (w.findComponent({ name: 'VSelect' }).props('items') as { value: string }[]).map(
      (i) => i.value,
    )
    expect(offered).toEqual(['', B.agent])
    expectNothingRaised()
    w.unmount()
  })
})

describe("The exec and SSH audit logs: the org's people (#4257)", () => {
  it("ExecAuditSection mounted for org B while the store holds org A's members: asks for org B's, and names org B's people", async () => {
    await useAgentStore().fetchTenantMembers(ORG_A)
    const since = asked().length
    const w = mount(ExecAuditSection, {
      props: { tenantId: ORG_B },
      global: globalOpts(),
      attachTo: document.body,
    })
    await settle()
    expect(asked().slice(since)).toContain(`/tenant/${ORG_B}/member`)
    expect(w.text()).toContain(B.lead)
    expectNothingRaised()
    w.unmount()
  })

  it("SshAuditSection mounted for org B while the store holds org A's members: asks for org B's, and names org B's people", async () => {
    await useAgentStore().fetchTenantMembers(ORG_A)
    const since = asked().length
    const w = mount(SshAuditSection, {
      props: { tenantId: ORG_B },
      global: globalOpts(),
      attachTo: document.body,
    })
    await settle()
    expect(asked().slice(since)).toContain(`/tenant/${ORG_B}/member`)
    expect(w.text()).toContain(B.lead)
    expectNothingRaised()
    w.unmount()
  })
})

describe("The exec and SSH policy dialogs: the org's people (#4257)", () => {
  const device = {
    id: B.agent,
    name: 'org-b-box',
    os: 'linux',
    agent_version: '0.4.189',
    is_online: true,
    presence: 'online',
  } as unknown as Agent
  const USERS_PICKER = 'Restrict to these users (empty = anyone with the permission)'
  function offeredUsers(w: VueWrapper): string[] {
    const picker = w
      .findAllComponents(components.VSelect)
      .find((s) => s.props('label') === USERS_PICKER)
    if (!picker) throw new Error('the dialog shows no user picker')
    return (picker.props('items') as { value: string }[]).map((i) => i.value)
  }

  it("ExecPolicyDialog opened for org B while the store holds org A's members: asks for org B's, and offers them", async () => {
    await useAgentStore().fetchTenantMembers(ORG_A)
    const since = asked().length
    const w = mount(ExecPolicyDialog, {
      props: { modelValue: true, tenantId: ORG_B, agent: device },
      global: globalOpts(),
      attachTo: document.body,
    })
    await settle()
    expect(asked().slice(since)).toContain(`/tenant/${ORG_B}/member`)
    expect(offeredUsers(w)).toEqual([B.user])
    expectNothingRaised()
    w.unmount()
  })

  it("SshPolicyDialog opened for org B while the store holds org A's members: asks for org B's, and offers them", async () => {
    await useAgentStore().fetchTenantMembers(ORG_A)
    const since = asked().length
    const w = mount(SshPolicyDialog, {
      props: { modelValue: true, tenantId: ORG_B, agent: device },
      global: globalOpts(),
      attachTo: document.body,
    })
    await settle()
    expect(asked().slice(since)).toContain(`/tenant/${ORG_B}/member`)
    expect(offeredUsers(w)).toEqual([B.user])
    expectNothingRaised()
    w.unmount()
  })
})

describe('The audit logs: the org changes (#4257)', () => {
  // Shape 1. Each log is a routed section (`props: true`), so a change of org
  // in the URL (back and forward, a link) reuses it with a new `tenantId`.
  // Each loaded once, on mount: it went on listing org A's log under org B,
  // and an answer for org A that landed late replaced whatever was there.
  type Org = ReturnType<typeof makeOrg>
  interface Log {
    name: string
    /** The log's GET, after the org. */
    route: string
    /** Text only org `o`'s row shows, whichever org's people the store holds. */
    row: (o: Org) => string
    /** It names people from the org's members. */
    people: boolean
    /** It names devices from the org's devices, which it loads itself. */
    devices: boolean
    mountAt: (tenantId: string) => VueWrapper
  }
  const at = (tenantId: string) => ({
    props: { tenantId },
    global: globalOpts(),
    attachTo: document.body,
  })
  const LOGS: Log[] = [
    {
      name: 'ExecAuditSection',
      route: 'exec-audit',
      row: (o) => `${o.tag} uptime`,
      people: true,
      devices: false,
      mountAt: (t) => mount(ExecAuditSection, at(t)),
    },
    {
      name: 'SshAuditSection',
      route: 'ssh-audit',
      row: (o) => `${o.tag} cli`,
      people: true,
      devices: false,
      mountAt: (t) => mount(SshAuditSection, at(t)),
    },
    {
      name: 'SshActivitySection',
      route: 'ssh-activity',
      row: (o) => `${o.tag} uptime`,
      people: false,
      devices: false,
      mountAt: (t) => mount(SshActivitySection, at(t)),
    },
    {
      name: 'ExternalAuditSection',
      route: 'external-rc-audit',
      row: (o) => `${o.tag} visitor`,
      people: false,
      devices: true,
      mountAt: (t) => mount(ExternalAuditSection, at(t)),
    },
  ]
  const toOrgB = (w: VueWrapper) =>
    w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])

  for (const log of LOGS) {
    it(`${log.name} follows its org: asks for org B's log, and lists only org B's rows`, async () => {
      const w = log.mountAt(ORG_A)
      await settle()
      expect(w.text()).toContain(log.row(A))

      const since = asked().length
      await toOrgB(w)
      await settle()
      const after = asked().slice(since)
      expect(after.filter(getOf(ORG_B, `${log.route}?`))).toHaveLength(1)
      expect(after.filter((p) => p.includes(ORG_A))).toEqual([])
      if (log.people) expect(after).toContain(`/tenant/${ORG_B}/member`)
      if (log.devices) expect(after).toContain(`/tenant/${ORG_B}/agent?per_page=100`)
      expect(w.text()).toContain(log.row(B))
      expect(w.text()).not.toContain(log.row(A))
      if (log.people) expect(w.text()).toContain(B.lead)
      expectNothingRaised()
      w.unmount()
    })

    it(`${log.name} drops org A's log when it answers after the change`, async () => {
      const late = hold(getOf(ORG_A, `${log.route}?`))
      const w = log.mountAt(ORG_A)
      await settle()
      expect(late.taken).toBe(true)

      await toOrgB(w)
      await settle()
      late.release()
      await settle()
      expect(w.text()).toContain(log.row(B))
      expect(w.text()).not.toContain(log.row(A))
      expectNothingRaised()
      w.unmount()
    })

    it(`${log.name} shows no error for org A's refusal that lands after the change`, async () => {
      const late = hold(getOf(ORG_A, `${log.route}?`), 'org A refused')
      const w = log.mountAt(ORG_A)
      await settle()
      expect(late.taken).toBe(true)

      await toOrgB(w)
      await settle()
      late.release()
      await settle()
      expect(w.text()).not.toContain('org A refused')
      expect(w.text()).toContain(log.row(B))
      expectNothingRaised()
      w.unmount()
    })

    it(`${log.name} lists none of org A's rows while org B's are on their way`, async () => {
      const w = log.mountAt(ORG_A)
      await settle()
      expect(w.text()).toContain(log.row(A))

      const next = hold(getOf(ORG_B, `${log.route}?`))
      await toOrgB(w)
      await settle()
      expect(next.taken).toBe(true)
      expect(w.text()).not.toContain(log.row(A))
      next.release()
      await settle()
      expect(w.text()).toContain(log.row(B))
      expectNothingRaised()
      w.unmount()
    })
  }
})

describe("The external audit page's people card: the org changes (#4257)", () => {
  // ExternalPeopleCard is the top of the external audit page, mounted with the
  // page's org as a prop. It loaded the org's outsiders once, on mount, so
  // under org B it went on listing org A's people, each with a Bar button.
  const people = () =>
    [...document.querySelectorAll('[data-testid="external-person"]')]
      .map((r) => r.textContent ?? '')
      .join('\n')
  const mountPage = (tenantId: string) =>
    mount(ExternalAuditSection, {
      props: { tenantId },
      global: globalOpts(),
      attachTo: document.body,
    })
  const toOrgB = (w: VueWrapper) =>
    w.setProps({ tenantId: ORG_B } as unknown as Parameters<typeof w.setProps>[0])

  it("asks for org B's people, and lists only org B's", async () => {
    const w = mountPage(ORG_A)
    await settle()
    expect(people()).toContain('Org A outsider')

    const since = asked().length
    await toOrgB(w)
    await settle()
    const after = asked().slice(since)
    expect(after.filter(getOf(ORG_B, 'external-principals?'))).toHaveLength(1)
    expect(after.filter((p) => p.includes(ORG_A))).toEqual([])
    expect(people()).toContain('Org B outsider')
    expect(people()).not.toContain('Org A outsider')
    expectNothingRaised()
    w.unmount()
  })

  it("drops org A's people when they answer after the change", async () => {
    const late = hold(getOf(ORG_A, 'external-principals?'))
    const w = mountPage(ORG_A)
    await settle()
    expect(late.taken).toBe(true)

    await toOrgB(w)
    await settle()
    late.release()
    await settle()
    expect(people()).toContain('Org B outsider')
    expect(people()).not.toContain('Org A outsider')
    expectNothingRaised()
    w.unmount()
  })

  it('a Bar sent under org A and refused after the change shows nothing under org B', async () => {
    let refuse: (e: Error) => void = () => {
      throw new Error('the Bar was never sent')
    }
    mockApi.put.mockImplementation(
      () =>
        new Promise((_resolve, reject) => {
          refuse = reject
        }),
    )
    const w = mountPage(ORG_A)
    await settle()
    await w.find('[data-testid="external-bar"]').trigger('click')
    await settle()
    expect(mockApi.put).toHaveBeenCalledWith(
      `/tenant/${ORG_A}/agent/${A.agent}/external-principals/${A.user}`,
      { revoked: true },
    )

    await toOrgB(w)
    await settle()
    refuse(new Error('org A refused'))
    await settle()
    expect(w.text()).not.toContain('org A refused')
    expect(people()).toContain('Org B outsider')
    expect(people()).not.toContain('Org A outsider')
    expectNothingRaised()
    w.unmount()
  })
})
