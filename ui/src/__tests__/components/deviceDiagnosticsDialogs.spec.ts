// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// A device's crash reports and uploaded logs are MANAGE_AGENTS on the server.
// The store answers `null` for a caller who may not read them (known from the
// mask, or the server's 403), and both dialogs explain that. Neither shows it
// as an error, nor as an empty list that would read as "nothing happened".
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
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

  // #4189 — a batch as `GET …/agent/{aid}/logs` answers it: each line's `ts`
  // is an RFC 3339 string. The server used to send bson's `{"$date": …}`,
  // which the time column printed raw and Copy wrote as `[object Object]`.
  describe("a line's time", () => {
    const TS = '2026-11-21T06:43:29.123Z'
    const line = {
      ts: TS,
      level: 'INFO',
      target: 'roomlerd::peer',
      msg: 'input dispatch',
      fields: { px: 1181 },
    }
    const batches = [
      {
        id: 'b00000000000000000000001',
        source: 'agent',
        agentId: 'c00000000000000000000001',
        userId: null,
        sessionId: null,
        hostIdHash: null,
        agentVersion: '0.4.230',
        lineCount: 1,
        createdAt: '2026-11-21T06:43:31.500Z',
        lines: [line],
      },
    ]

    let clipboard: PropertyDescriptor | undefined
    const writeText = vi.fn()
    beforeEach(() => {
      clipboard = Object.getOwnPropertyDescriptor(navigator, 'clipboard')
      writeText.mockReset().mockResolvedValue(undefined)
      Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true })
    })
    afterEach(() => {
      if (clipboard) Object.defineProperty(navigator, 'clipboard', clipboard)
      else delete (navigator as unknown as Record<string, unknown>).clipboard
    })

    it('shows as the time of day, to the millisecond', async () => {
      await open(AgentLogsDialog, storeAnswering(null, batches))
      const cells = Array.from(document.body.querySelectorAll('.log-ts')).map((c) =>
        (c.textContent ?? '').trim(),
      )
      const local = new Date(TS).toLocaleTimeString(undefined, { hour12: false })
      expect(cells).toEqual([`${local}.123`])
      expect(cells[0]).toMatch(/^\d{1,2}:\d{2}:\d{2}\.123$/)
    })

    it('is what Copy writes, ahead of the level, the target and the message', async () => {
      await open(AgentLogsDialog, storeAnswering(null, batches))
      const copy = Array.from(document.body.querySelectorAll('button')).find((b) =>
        (b.textContent ?? '').includes('Copy visible'),
      )
      expect(copy).toBeDefined()
      copy?.click()
      await flushPromises()
      expect(writeText).toHaveBeenCalledTimes(1)
      expect(writeText).toHaveBeenCalledWith(`${TS} INFO roomlerd::peer input dispatch px=1181`)
    })
  })
})
