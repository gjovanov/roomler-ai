// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// A device's crash reports and uploaded logs are MANAGE_AGENTS on the server.
// The store answers `null` for a caller who may not read them (known from the
// mask, or the server's 403), and both dialogs explain that. Neither shows it
// as an error, nor as an empty list that would read as "nothing happened".
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { reactive, type Component } from 'vue'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

const hoisted = vi.hoisted(() => ({ agent: {} as Record<string, unknown> }))

vi.mock('@/stores/agents', () => ({ useAgentStore: () => hoisted.agent }))

import AgentCrashesDialog from '@/components/admin/AgentCrashesDialog.vue'
import AgentLogsDialog from '@/components/admin/AgentLogsDialog.vue'

const vuetify = createVuetify({ components, directives })

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
      width: 1024,
      height: 768,
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

let wrapper: VueWrapper | null = null
afterEach(() => {
  wrapper?.unmount()
  wrapper = null
  document.body.innerHTML = ''
})

/** The store's two fetchers, answering as given (`null` = refused). */
function storeAnswering(crashes: unknown, logs: unknown) {
  return reactive({
    fetchCrashes: vi.fn().mockResolvedValue(crashes),
    fetchLogs: vi.fn().mockResolvedValue(logs),
  })
}

/** Mount closed, then open, as the Devices page does: the dialogs fetch on
 *  the open. Their content is teleported to <body>, outside the wrapper. */
async function open(component: Component, store: Record<string, unknown>) {
  hoisted.agent = store
  wrapper = mount(component, {
    props: {
      modelValue: false,
      tenantId: 'aaaaaaaaaaaaaaaaaaaaaaaa',
      agentId: 'c00000000000000000000001',
      agentName: 'build-01',
    },
    global: { plugins: [vuetify] },
    attachTo: document.body,
  })
  await wrapper.setProps({ modelValue: true })
  await flushPromises()
}

function shown(): string {
  return (document.body.textContent ?? '').replace(/\s+/g, ' ')
}

function alerts(): number {
  return document.body.querySelectorAll('.v-alert').length
}

describe('the crash reports dialog', () => {
  it('explains a refusal, and shows neither an error nor "no crashes"', async () => {
    await open(AgentCrashesDialog, storeAnswering(null, null))
    expect(document.body.querySelector('[data-testid="agent-crashes-refused"]')).not.toBeNull()
    expect(shown()).toContain('the Manage devices permission')
    expect(shown()).not.toContain('No crashes reported')
    expect(alerts()).toBe(1)
  })

  it('shows a holder the list, empty or not', async () => {
    await open(AgentCrashesDialog, storeAnswering([], null))
    expect(document.body.querySelector('[data-testid="agent-crashes-refused"]')).toBeNull()
    expect(shown()).toContain('No crashes reported')
  })
})

describe('the agent logs dialog', () => {
  it('explains a refusal, and shows neither an error nor "no logs yet"', async () => {
    await open(AgentLogsDialog, storeAnswering(null, null))
    expect(document.body.querySelector('[data-testid="agent-logs-refused"]')).not.toBeNull()
    expect(shown()).toContain('the Manage devices permission')
    expect(shown()).not.toContain('No logs uploaded yet')
    expect(alerts()).toBe(1)
  })

  it('shows a holder the log view, empty or not', async () => {
    await open(AgentLogsDialog, storeAnswering(null, []))
    expect(document.body.querySelector('[data-testid="agent-logs-refused"]')).toBeNull()
    expect(shown()).toContain('No logs uploaded yet')
  })
})
