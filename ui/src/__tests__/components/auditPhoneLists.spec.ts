// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-116 P3d — the audit lists on a phone. 100 entries as full cards were
// 43,398 px at 360 px on production: a phone now shows 20 at a time and a long
// command three lines until tapped. Wider screens keep every row. And a
// refusal without a message must never also read "granted" (a v-else paired
// with the wrong v-if, from P3a).
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
import type { ExecAuditEntry, SshAuditEntry } from '@/stores/agents'
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
    serve([
      sshEntry(0, { denied: 'no_permission' as SshAuditEntry['denied'], denied_message: null, account_mode: null }),
      sshEntry(1, { denied: 'no_permission' as SshAuditEntry['denied'], denied_message: 'You may not SSH here', account_mode: null }),
      sshEntry(2),
    ])
    const w = sshAt(360)
    await flushPromises()
    const results = w.findAll('td[data-label="Result"]').map((td) => td.text())
    expect(results[0]).not.toContain('granted')
    expect(results[1]).not.toContain('granted')
    expect(results[1]).toContain('You may not SSH here')
    expect(results[2]).toContain('granted')
  })
})

describe('the exec audit on a phone (FR-116 P3d)', () => {
  const long = Array.from({ length: 30 }, (_, i) => `step-${i} --flag value`).join(' && ')

  it('clamps a long command until tapped, and only a long one offers to', async () => {
    serve([], [execEntry(0, long), execEntry(1, 'uptime')])
    const w = execAt(360)
    await flushPromises()
    const cells = w.findAll('td[data-label="Command"]')
    const toggle = cells[0].get('button')
    expect(toggle.attributes('aria-expanded')).toBe('false')
    expect(cells[0].get('code').classes()).not.toContain('audit-cmd--open')
    await toggle.trigger('click')
    expect(cells[0].get('button').attributes('aria-expanded')).toBe('true')
    expect(cells[0].get('code').classes()).toContain('audit-cmd--open')
    expect(cells[1].find('button').exists()).toBe(false)
  })

  it('offers no toggle on a wider screen, where the whole command is in its title', async () => {
    serve([], [execEntry(0, long)])
    const w = execAt(1300)
    await flushPromises()
    const cell = w.get('td[data-label="Command"]')
    expect(cell.find('button').exists()).toBe(false)
    expect(cell.get('code').attributes('title')).toBe(long)
  })
})
