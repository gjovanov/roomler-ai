// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-116 P3d — the audit lists on a phone. 100 entries as full cards were
// 43,398 px at 360 px on production: a phone now shows 20 at a time. Wider
// screens keep every row. And a refusal without a message must never also read
// "granted" (a v-else paired with the wrong v-if, from P3a).
//
// FR-120 P4 — a phone card shows what decides (the device, when, the result,
// and an exec entry's command clamped to two lines) and opens on a tap for the
// rest; six labelled lines a card made the audits 3.3–3.6× their desktop
// height.
import { describe, it, expect, vi, beforeAll, beforeEach, afterEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { mount, flushPromises } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

beforeAll(() => {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver
})

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import SshAuditSection from '@/components/admin/SshAuditSection.vue'
import ExecAuditSection from '@/components/admin/ExecAuditSection.vue'
import ExternalAuditSection from '@/components/admin/ExternalAuditSection.vue'
import { useTenantStore } from '@/stores/tenant'
import type { ExecAuditEntry, SshAuditEntry } from '@/stores/agents'
import type { ExternalAuditEntry, ExternalPrincipal } from '@/utils/externalAudit'
import { PERMISSION_FLAGS } from '@/utils/permissions'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const realWidth = window.innerWidth

function sshEntry(i: number, over: Partial<SshAuditEntry> = {}): SshAuditEntry {
  return {
    id: `s${i}`,
    tenant_id: 't1',
    agent_id: 'a1',
    user_id: 'u1',
    source: 'cli',
    caller: 'u1',
    account_mode: 'console_user',
    at: new Date(1_700_000_000_000 - i * 60_000).toISOString(),
    ...over,
  }
}

function execEntry(i: number, command: string): ExecAuditEntry {
  return {
    tenant_id: 't1',
    agent_id: 'a1',
    user_id: 'u1',
    request_id: `r${i}`,
    source: 'cli',
    shell: 'sh',
    command,
    at: new Date(1_700_000_000_000 - i * 60_000).toISOString(),
    exit_code: 0,
  }
}

function serve(ssh: SshAuditEntry[], exec: ExecAuditEntry[] = []) {
  mockApi.get.mockImplementation(async (url: string) => {
    if (url.includes('ssh-audit')) return { items: ssh, total: ssh.length } as never
    if (url.includes('exec-audit')) return { items: exec, total: exec.length } as never
    return { items: [] } as never
  })
}

/** Vuetify reads the window's width when it is created: a phone is 360 px. */
function vuetifyAt(width: number) {
  Object.defineProperty(window, 'innerWidth', { value: width, configurable: true })
  return createVuetify({ components, directives })
}
const sshAt = (width: number) =>
  mount(SshAuditSection, { props: { tenantId: 't1' }, global: { plugins: [vuetifyAt(width)] } })
const execAt = (width: number) =>
  mount(ExecAuditSection, { props: { tenantId: 't1' }, global: { plugins: [vuetifyAt(width)] } })

beforeEach(() => {
  setActivePinia(createPinia())
  vi.clearAllMocks()
})

afterEach(() => {
  Object.defineProperty(window, 'innerWidth', { value: realWidth, configurable: true })
})

describe('the SSH audit on a phone (FR-116 P3d)', () => {
  const entries = Array.from({ length: 45 }, (_, i) => sshEntry(i))

  it('shows 20 cards, then 20 more, then the last 5', async () => {
    serve(entries)
    const w = sshAt(360)
    await flushPromises()
    expect(w.findAll('tbody tr')).toHaveLength(20)
    await w.get('[data-testid="audit-show-more"]').trigger('click')
    expect(w.findAll('tbody tr')).toHaveLength(40)
    expect(w.get('[data-testid="audit-show-more"]').text()).toBe('Show 5 more')
    await w.get('[data-testid="audit-show-more"]').trigger('click')
    expect(w.findAll('tbody tr')).toHaveLength(45)
    expect(w.find('[data-testid="audit-show-more"]').exists()).toBe(false)
  })

  it('keeps every row on a wider screen', async () => {
    serve(entries)
    const w = sshAt(1300)
    await flushPromises()
    expect(w.findAll('tbody tr')).toHaveLength(45)
    expect(w.find('[data-testid="audit-show-more"]').exists()).toBe(false)
  })

  it('never reads "granted" on a refusal, with or without its message', async () => {
    const refusals = [
      sshEntry(0, { denied: 'no_permission' as SshAuditEntry['denied'], denied_message: null, account_mode: null }),
      sshEntry(1, { denied: 'no_permission' as SshAuditEntry['denied'], denied_message: 'You may not SSH here', account_mode: null }),
      sshEntry(2),
    ]
    // A phone: the card's summary carries the result, and the opened card the
    // server's reason.
    serve(refusals)
    const phone = sshAt(360)
    await flushPromises()
    const summaries = phone.findAll('[data-testid="audit-row-toggle"]').map((b) => b.text())
    expect(summaries[0]).not.toContain('granted')
    expect(summaries[1]).not.toContain('granted')
    expect(summaries[2]).toContain('granted')
    const why = phone.findAll('td[data-label="Why"]')
    expect(why).toHaveLength(1)
    expect(why[0].text()).toBe('You may not SSH here')
    // A desktop: the Result column.
    serve(refusals)
    const desk = sshAt(1300)
    await flushPromises()
    const results = desk.findAll('td[data-label="Result"]').map((td) => td.text())
    expect(results[0]).not.toContain('granted')
    expect(results[1]).not.toContain('granted')
    expect(results[2]).toContain('granted')
  })

  it('shows what decides on a phone, and opens a card on a tap for the rest (FR-120 P4)', async () => {
    serve([sshEntry(0), sshEntry(1)])
    const w = sshAt(360)
    await flushPromises()
    const rows = w.findAll('tbody tr')
    // The summary is the device, when and the result; the When, Device and
    // Result columns are not repeated below it.
    expect(rows[0].find('[data-testid="audit-row-toggle"]').exists()).toBe(true)
    expect(rows[0].find('td[data-label="When"]').exists()).toBe(false)
    expect(rows[0].find('td[data-label="Result"]').exists()).toBe(false)
    // Who, Via and As are details: closed until the card is tapped.
    for (const label of ['Who', 'Via', 'As']) {
      expect(rows[0].get(`td[data-label="${label}"]`).classes()).toContain('stack-detail')
    }
    expect(rows[0].classes()).not.toContain('stack-open')
    await rows[0].get('[data-testid="audit-row-toggle"]').trigger('click')
    const opened = w.findAll('tbody tr')
    expect(opened[0].classes()).toContain('stack-open')
    expect(opened[0].get('[data-testid="audit-row-toggle"]').attributes('aria-expanded')).toBe('true')
    // Each card opens on its own.
    expect(opened[1].classes()).not.toContain('stack-open')
  })

  it('keeps its columns on a wider screen, with no summary', async () => {
    serve([sshEntry(0)])
    const w = sshAt(1300)
    await flushPromises()
    expect(w.find('[data-testid="audit-row-toggle"]').exists()).toBe(false)
    expect(w.findAll('tbody tr td').map((td) => td.attributes('data-label'))).toEqual(['When', 'Device', 'Who', 'Via', 'As', 'Result'])
  })
})

describe('the exec audit on a phone (FR-116 P3d)', () => {
  const long = Array.from({ length: 30 }, (_, i) => `step-${i} --flag value`).join(' && ')

  it('clamps a command until its card is tapped, which opens the rest too (FR-120 P4)', async () => {
    serve([], [execEntry(0, long), execEntry(1, 'uptime')])
    const w = execAt(360)
    await flushPromises()
    const rows = w.findAll('tbody tr')
    const toggle = rows[0].get('[data-testid="audit-row-toggle"]')
    expect(toggle.attributes('aria-expanded')).toBe('false')
    expect(toggle.text()).toContain('ok')
    expect(rows[0].get('code').classes()).not.toContain('audit-cmd--open')
    expect(rows[0].get('td[data-label="Who"]').classes()).toContain('stack-detail')
    // The old per-command "Show all" is gone: the card's own toggle opens it.
    expect(rows[0].get('td[data-label="Command"]').find('button').exists()).toBe(false)
    await toggle.trigger('click')
    const opened = w.findAll('tbody tr')
    expect(opened[0].classes()).toContain('stack-open')
    expect(opened[0].get('[data-testid="audit-row-toggle"]').attributes('aria-expanded')).toBe('true')
    expect(opened[0].get('code').classes()).toContain('audit-cmd--open')
    expect(opened[1].get('code').classes()).not.toContain('audit-cmd--open')
  })

  it('spells out the identity a command ran as, which a desktop keeps in a title', async () => {
    const entry = { ...execEntry(0, 'whoami'), ran_as: 'system' } as ExecAuditEntry
    serve([], [entry])
    const phone = execAt(360)
    await flushPromises()
    expect(phone.get('td[data-label="Ran as"]').text()).toBe("Ran as SYSTEM through the device's service")
    serve([], [entry])
    const desk = execAt(1300)
    await flushPromises()
    expect(desk.find('td[data-label="Ran as"]').exists()).toBe(false)
  })

  it('offers no toggle on a wider screen, where the whole command is in its title', async () => {
    serve([], [execEntry(0, long)])
    const w = execAt(1300)
    await flushPromises()
    expect(w.find('[data-testid="audit-row-toggle"]').exists()).toBe(false)
    const cell = w.get('td[data-label="Command"]')
    expect(cell.find('button').exists()).toBe(false)
    expect(cell.get('code').attributes('title')).toBe(long)
    expect(w.findAll('tbody tr td').map((td) => td.attributes('data-label'))).toEqual(['When', 'Device', 'Who', 'Via', 'Command', 'Result'])
  })
})

describe('the external access audit (FR-52 P5)', () => {
  const logRow = (i: number, over: Partial<ExternalAuditEntry> = {}): ExternalAuditEntry => ({
    id: `e${i}`,
    action: 'login',
    agent_id: 'a1',
    user_id: 'aaaaaaaaaaaaaaaaaa000001',
    actor: 'Alice from IT',
    at: new Date(1_700_000_000_000 - i * 60_000).toISOString(),
    ...over,
  })
  const person = (over: Partial<ExternalPrincipal> = {}): ExternalPrincipal => ({
    agent_id: 'a1',
    user_id: 'aaaaaaaaaaaaaaaaaa000001',
    name: 'Alice from IT',
    first_session_at: '2026-10-30T10:00:00Z',
    last_session_at: '2026-10-31T10:00:00Z',
    revoked_at: null,
    ...over,
  })
  function serveExternal(log: ExternalAuditEntry[], people: ExternalPrincipal[] = []) {
    mockApi.get.mockImplementation(async (url: string) => {
      if (url.includes('external-rc-audit')) return { items: log, total: log.length } as never
      if (url.includes('external-principals')) return { items: people, total: people.length } as never
      return { items: [] } as never
    })
  }
  /** Who is reading, by their permission mask. */
  const readAs = (permissions: number) => useTenantStore().$patch({ myPermissions: permissions })
  const externalAt = (width: number) =>
    mount(ExternalAuditSection, {
      props: { tenantId: 't1' },
      global: { plugins: [vuetifyAt(width)] },
    })
  const logRows = (w: ReturnType<typeof externalAt>) =>
    w.findAll('tbody tr').filter((r) => r.attributes('data-testid') !== 'external-person')

  const MANAGE_AGENTS = PERMISSION_FLAGS.find((f) => f.key === 'MANAGE_AGENTS')!.bit
  const REMOTE_CONTROL = PERMISSION_FLAGS.find((f) => f.key === 'REMOTE_CONTROL')!.bit

  it('shows 20 cards a time on a phone, and every row on a wider screen', async () => {
    const rows = Array.from({ length: 45 }, (_, i) => logRow(i))
    serveExternal(rows)
    readAs(MANAGE_AGENTS)
    const phone = externalAt(360)
    await flushPromises()
    expect(logRows(phone)).toHaveLength(20)
    await phone.get('[data-testid="audit-show-more"]').trigger('click')
    expect(logRows(phone)).toHaveLength(40)

    serveExternal(rows)
    const wide = externalAt(1300)
    await flushPromises()
    expect(logRows(wide)).toHaveLength(45)
  })

  it('a refused login never reads as one that went through', async () => {
    serveExternal([
      logRow(0, {
        login_refused: 'unavailable',
        login_detail: 'an admin of the org barred this person from the device',
      }),
    ])
    readAs(MANAGE_AGENTS)
    const w = externalAt(1300)
    await flushPromises()
    const result = w.get('td[data-label="Result"]')
    expect(result.text()).toBe('refused')
    expect(result.get('.v-chip').attributes('title')).toContain('barred this person')
  })

  it('a bar says who it was about, and the account behind a name', async () => {
    serveExternal(
      [logRow(0, { action: 'principal', approved: false, actor: 'An Admin', principal: 'bbbbbbbbbbbbbbbbbb000002' })],
      [person(), person({ agent_id: 'a2', user_id: 'cccccccccccccccccc000003', name: 'Alice from IT' })],
    )
    readAs(MANAGE_AGENTS)
    const w = externalAt(1300)
    await flushPromises()
    expect(w.text()).toContain('about · 000002')
    expect(w.text()).toContain('outside · 000001')
    expect(w.text()).toContain('outside · 000003')
  })

  it('Bar asks the server, and the log reads again after', async () => {
    serveExternal([], [person()])
    readAs(MANAGE_AGENTS)
    mockApi.put.mockResolvedValue({ revoked: true } as never)
    const w = externalAt(1300)
    await flushPromises()
    mockApi.get.mockClear()
    await w.get('[data-testid="external-bar"]').trigger('click')
    await flushPromises()
    expect(mockApi.put).toHaveBeenCalledWith(
      '/tenant/t1/agent/a1/external-principals/aaaaaaaaaaaaaaaaaa000001',
      { revoked: true },
    )
    const reads = mockApi.get.mock.calls.map(([url]) => url as string)
    expect(reads.some((u) => u.includes('external-principals'))).toBe(true)
    expect(reads.some((u) => u.includes('external-rc-audit'))).toBe(true)
  })

  it('a refusal shows its message and leaves the page where it is', async () => {
    serveExternal([], [person({ revoked_at: '2026-10-31T11:00:00Z' })])
    readAs(MANAGE_AGENTS | REMOTE_CONTROL)
    mockApi.put.mockRejectedValue(new Error('granting external access also requires REMOTE_CONTROL'))
    const w = externalAt(1300)
    await flushPromises()
    await w.get('[data-testid="external-let-back"]').trigger('click')
    await flushPromises()
    expect(w.text()).toContain('also requires REMOTE_CONTROL')
    expect(w.findAll('[data-testid="external-person"]')).toHaveLength(1)
  })

  it('only offers what this reader may do', async () => {
    serveExternal([], [person(), person({ agent_id: 'a2', revoked_at: '2026-10-31T11:00:00Z' })])

    readAs(0)
    const reader = externalAt(1300)
    await flushPromises()
    expect(reader.find('[data-testid="external-bar"]').exists()).toBe(false)
    expect(reader.find('[data-testid="external-let-back"]').exists()).toBe(false)

    readAs(MANAGE_AGENTS)
    const admin = externalAt(1300)
    await flushPromises()
    expect(admin.find('[data-testid="external-bar"]').exists()).toBe(true)
    expect(admin.find('[data-testid="external-let-back"]').exists()).toBe(false)

    readAs(MANAGE_AGENTS | REMOTE_CONTROL)
    const both = externalAt(1300)
    await flushPromises()
    expect(both.find('[data-testid="external-let-back"]').exists()).toBe(true)
  })

  it('says what a bar stops, so nobody takes it for a locked door', async () => {
    serveExternal([], [person()])
    readAs(MANAGE_AGENTS)
    const w = externalAt(1300)
    await flushPromises()
    expect(w.text()).toContain('It stops an account, not a person')
    expect(w.text()).toContain('change the password on the device')
  })

  it('names an account that has gone, and a device it cannot name by its id', async () => {
    serveExternal([], [person({ name: null })])
    readAs(MANAGE_AGENTS)
    const w = externalAt(1300)
    await flushPromises()
    expect(w.text()).toContain('Account removed')
    expect(w.get('[data-testid="external-person"] td[data-label="Device"]').text()).toBe('a1')
  })
})
