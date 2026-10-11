// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-92 P7 — the permanent keep-busy history, as the org's keep-busy card
// shows it: each row says what happened, on which device (or the org), who
// did it, and whether that is the server's own record or the device's
// claim. "Load more" pages by the last row's instant.
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'

const hoisted = vi.hoisted(() => ({
  fetchKeepBusyHistory: vi.fn(),
  fetchAgents: vi.fn(),
}))
vi.mock('@/stores/agents', () => ({
  useAgentStore: () => ({
    fetchKeepBusyHistory: hoisted.fetchKeepBusyHistory,
    fetchAgents: hoisted.fetchAgents,
    agents: [{ id: 'a1', name: 'laptop-01', display_name: 'The laptop' }],
  }),
}))

import KeepBusyHistorySection from '@/components/admin/KeepBusyHistorySection.vue'

const vuetify = createVuetify({ components, directives })
const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })

beforeAll(() => {
  const g = globalThis as unknown as Record<string, unknown>
  if (!('ResizeObserver' in g)) {
    g.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    }
  }
})

afterEach(() => {
  vi.clearAllMocks()
})

function row(over: Record<string, unknown> = {}) {
  return {
    id: 'e1',
    tenant_id: 't1',
    at: '2026-10-11T12:00:00.000Z',
    kind: 'on',
    source: 'device',
    agent_id: 'a1',
    agent_name: 'laptop-01',
    actor_name: 'Alice',
    pattern: 'heart',
    ...over,
  }
}

function render() {
  return mount(KeepBusyHistorySection, {
    props: { tenantId: 't1' },
    global: { plugins: [vuetify, i18n] },
  })
}

describe('KeepBusyHistorySection (FR-92 P7)', () => {
  it('shows each row as what, where, who — and whose word it is', async () => {
    hoisted.fetchKeepBusyHistory.mockResolvedValueOnce([
      row(),
      row({ id: 'e2', kind: 'off', reason: 'stopped_by_controller', actor_name: undefined, pattern: undefined }),
      row({
        id: 'e3',
        kind: 'org_denied',
        source: 'server',
        agent_id: undefined,
        agent_name: undefined,
        actor_name: 'The owner',
        actor_user_id: 'u9',
        pattern: undefined,
      }),
      row({ id: 'e4', kind: 'org_default_pattern', source: 'server', agent_id: undefined, agent_name: undefined, actor_name: 'The owner', pattern: undefined }),
    ])
    const w = render()
    await flushPromises()
    expect(hoisted.fetchKeepBusyHistory).toHaveBeenCalledWith('t1', { agentId: undefined, before: undefined, limit: 50 })
    const rows = w.findAll('[data-testid="keep-busy-history-row"]')
    expect(rows).toHaveLength(4)
    expect(rows[0].text()).toContain('turned on · heart')
    expect(rows[0].text()).toContain('laptop-01')
    expect(rows[0].text()).toContain('Alice')
    expect(rows[0].find('[data-testid="keep-busy-history-source-device"]').text()).toBe('device-reported')
    expect(rows[1].text()).toContain('turned off · stopped by controller')
    expect(rows[2].text()).toContain('denied for the organization')
    expect(rows[2].text()).toContain('Organization')
    expect(rows[2].text()).toContain('The owner')
    expect(rows[2].find('[data-testid="keep-busy-history-source-server"]').text()).toBe('admin')
    // A cleared default reads as the product default, not as a blank.
    expect(rows[3].text()).toContain('default pattern changed · circle (the default)')
    expect(w.find('[data-testid="keep-busy-history-more"]').exists()).toBe(false)
    w.unmount()
  })

  it('names a removed device by the name written on the row, and a current one by its label', async () => {
    hoisted.fetchKeepBusyHistory.mockResolvedValueOnce([
      row({ id: 'e1', agent_id: 'gone', agent_name: 'old-box' }),
      row({ id: 'e2', agent_id: 'a1', agent_name: undefined }),
    ])
    const w = render()
    await flushPromises()
    const rows = w.findAll('[data-testid="keep-busy-history-row"]')
    expect(rows[0].text()).toContain('old-box')
    expect(rows[1].text()).toContain('The laptop')
    w.unmount()
  })

  it('offers "Load more" after a full page and pages by the last row’s instant', async () => {
    const page = Array.from({ length: 50 }, (_, i) =>
      row({ id: `p${i}`, at: `2026-10-11T12:00:${String(59 - i).padStart(2, '0')}.000Z` }),
    )
    hoisted.fetchKeepBusyHistory.mockResolvedValueOnce(page)
    const w = render()
    await flushPromises()
    expect(w.findAll('[data-testid="keep-busy-history-row"]')).toHaveLength(50)
    const more = w.find('[data-testid="keep-busy-history-more"]')
    expect(more.exists()).toBe(true)
    hoisted.fetchKeepBusyHistory.mockResolvedValueOnce([row({ id: 'last', at: '2026-10-11T11:00:00.000Z' })])
    await more.trigger('click')
    await flushPromises()
    expect(hoisted.fetchKeepBusyHistory).toHaveBeenLastCalledWith('t1', {
      agentId: undefined,
      before: '2026-10-11T12:00:10.000Z',
      limit: 50,
    })
    expect(w.findAll('[data-testid="keep-busy-history-row"]')).toHaveLength(51)
    // A short page: that was the end.
    expect(w.find('[data-testid="keep-busy-history-more"]').exists()).toBe(false)
    w.unmount()
  })

  it('narrows to one device, with the tenant always in the ask', async () => {
    hoisted.fetchKeepBusyHistory.mockResolvedValue([])
    const w = render()
    await flushPromises()
    const select = w.findComponent({ name: 'VSelect' })
    select.vm.$emit('update:modelValue', 'a1')
    await flushPromises()
    expect(hoisted.fetchKeepBusyHistory).toHaveBeenLastCalledWith('t1', { agentId: 'a1', before: undefined, limit: 50 })
    expect(w.find('[data-testid="keep-busy-history-empty"]').exists()).toBe(true)
    w.unmount()
  })

  it('a refusal is shown in place and ends nothing', async () => {
    hoisted.fetchKeepBusyHistory.mockRejectedValueOnce(new Error('Missing MANAGE_AGENTS permission'))
    const w = render()
    await flushPromises()
    expect(w.text()).toContain('Missing MANAGE_AGENTS permission')
    expect(w.findAll('[data-testid="keep-busy-history-row"]')).toHaveLength(0)
    w.unmount()
  })
})
