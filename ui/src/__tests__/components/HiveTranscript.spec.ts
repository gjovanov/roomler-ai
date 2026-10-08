// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-90 P1a — an approval in a session's transcript: a driver sees Allow and
// Deny while the device says it is open; anyone else sees that it waits;
// once it is over, how it ended. What the model asked to run is text, never
// markup — the model writes it.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { ref, shallowRef } from 'vue'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'

const hoisted = vi.hoisted(() => ({
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  viewer: null as any,
}))
vi.mock('@/composables/useHiveViewer', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/composables/useHiveViewer')>()),
  useHiveViewer: () => hoisted.viewer,
}))

import HiveTranscript from '@/components/hive/HiveTranscript.vue'

const vuetify = createVuetify({ components, directives })

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function makeViewer(events: any[], pending: string[], mayAnswer: boolean) {
  return {
    status: ref('open'),
    reason: ref(null),
    events: shallowRef(events.map((event, i) => ({ seq: i + 1, ts: i, fence: 1, event }))),
    runState: ref(pending.length ? 'awaiting_approval' : 'running'),
    mayPrompt: ref(mayAnswer),
    mayAnswer: ref(mayAnswer),
    pendingApprovals: ref(pending),
    live: ref(true),
    hasEarlier: ref(false),
    open: vi.fn(),
    close: vi.fn(),
    prompt: vi.fn(() => Promise.resolve({ ok: true })),
    answer: vi.fn(() => Promise.resolve({ ok: true })),
    loadEarlier: vi.fn(),
  }
}

const asked = (id: string, input: unknown = { command: 'whoami' }) => ({
  kind: 'approval_requested',
  id,
  tool_name: 'Bash',
  tool_use_id: 'toolu_1',
  input,
})

async function render() {
  const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
  const w = mount(HiveTranscript, {
    props: { sessionId: 's1' },
    global: { plugins: [vuetify, i18n] },
  })
  await flushPromises()
  return w
}

const button = (w: Awaited<ReturnType<typeof render>>, id: string) => w.find(`[data-testid="${id}"]`)

beforeEach(() => {
  // The composer's auto-grow textarea measures itself; jsdom has no observer.
  vi.stubGlobal(
    'ResizeObserver',
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  )
})

afterEach(() => {
  hoisted.viewer = null
  vi.unstubAllGlobals()
})

describe('HiveTranscript — approvals (FR-90 P1a)', () => {
  it('lets a driver allow an open approval', async () => {
    hoisted.viewer = makeViewer([asked('a1')], ['a1'], true)
    const w = await render()
    const card = w.find('[data-approval="a1"]')
    expect(card.text()).toContain('Bash needs approval to run')
    expect(card.text()).toContain('"command": "whoami"')
    await button(w, 'hive-approve').trigger('click')
    await flushPromises()
    expect(hoisted.viewer.answer).toHaveBeenCalledWith('a1', 'allow', undefined)
  })

  it('sends a driver’s reason with a denial', async () => {
    hoisted.viewer = makeViewer([asked('a1')], ['a1'], true)
    const w = await render()
    await w.find('[data-testid="hive-deny-note"] input').setValue('  not on this box ')
    await button(w, 'hive-deny').trigger('click')
    await flushPromises()
    expect(hoisted.viewer.answer).toHaveBeenCalledWith('a1', 'deny', 'not on this box')
  })

  it('shows anyone else that it waits, with nothing to press', async () => {
    hoisted.viewer = makeViewer([asked('a1')], ['a1'], false)
    const w = await render()
    expect(button(w, 'hive-approve').exists()).toBe(false)
    expect(button(w, 'hive-deny').exists()).toBe(false)
    expect(w.find('[data-approval="a1"]').text()).toContain("Waiting for the session's starter to answer.")
  })

  it('takes the device’s word over the transcript: not open means no buttons', async () => {
    hoisted.viewer = makeViewer(
      [
        asked('a1'),
        { kind: 'approval_resolved', id: 'a1', outcome: 'allowed', by: 'Dev' },
        asked('a2'),
        { kind: 'approval_resolved', id: 'a2', outcome: 'denied', by: 'Dev', message: 'not now' },
        asked('a3'),
        { kind: 'approval_resolved', id: 'a3', outcome: 'expired' },
        asked('a4'),
      ],
      [],
      true,
    )
    const w = await render()
    expect(button(w, 'hive-approve').exists()).toBe(false)
    const ends = w.findAll('[data-testid="hive-approval-end"]').map((e) => e.text())
    expect(ends).toEqual([
      '✅ Allowed by Dev',
      '⛔ Denied by Dev: not now',
      '⌛ Nobody answered in time, so it did not run.',
    ])
    // Open nowhere and ended nowhere: the session ended under it.
    expect(w.find('[data-approval="a4"]').text()).toContain('Not answered.')
  })

  it('renders what the model asked to run as text, never as markup', async () => {
    hoisted.viewer = makeViewer([asked('a1', { command: '<img src=x onerror="alert(1)">' })], ['a1'], true)
    const w = await render()
    const card = w.find('[data-approval="a1"]')
    expect(card.find('img').exists()).toBe(false)
    expect(card.text()).toContain('<img src=x onerror=')
  })
})
