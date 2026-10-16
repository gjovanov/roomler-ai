// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-90 P2j-3 — a session deleted for good. Nothing is deleted until the
// owner types the session's title; a refusal is said in the server's words
// and deletes nothing; once it purges, each device is followed, its parts in
// words, read again every 5 s until every device answered, and never once the
// dialog closed.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'
import type { HivePurge, HiveSession } from '@/stores/hive'

const hoisted = vi.hoisted(() => ({
  purge: vi.fn(),
  fetchPurge: vi.fn(),
  fetchAgent: vi.fn(),
  agents: [
    { id: 'p', name: 'laptop' },
    { id: 'm', name: 'desktop', display_name: 'Big box' },
  ] as Array<{ id: string; name: string; display_name?: string }>,
}))
vi.mock('@/stores/hive', () => ({
  useHiveStore: () => ({ purge: hoisted.purge, fetchPurge: hoisted.fetchPurge }),
}))
vi.mock('@/stores/agents', () => ({
  useAgentStore: () => ({ agents: hoisted.agents, fetchAgent: hoisted.fetchAgent }),
}))

import HivePurgeDialog from '@/components/hive/HivePurgeDialog.vue'

const vuetify = createVuetify({ components, directives })
/** The dialog inline, its content there while it is open. */
const DialogStub = { props: ['modelValue'], template: '<div v-if="modelValue"><slot /></div>' }

const session = (over: Partial<HiveSession> = {}): HiveSession => ({
  id: 's1',
  owner_id: 'u1',
  title: 'Fix the kubelet',
  harness: 'claude-code',
  harness_session: 'h',
  device_id: 'p',
  device_name: 'laptop',
  folder: '/work/app',
  status: 'ended',
  fence: 2,
  created_at: '',
  updated_at: '',
  ...over,
})

const purge = (devices: HivePurge['devices'], done_at?: string): HivePurge => ({
  id: 'g1',
  session_id: 's1',
  reason: 'owner',
  at: '',
  ...(done_at ? { done_at } : {}),
  devices,
})

function render(s: HiveSession = session()) {
  const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
  return mount(HivePurgeDialog, {
    props: { modelValue: true, tenantId: 't1', session: s },
    global: { plugins: [vuetify, i18n], stubs: { VDialog: DialogStub } },
  })
}

const submit = (w: ReturnType<typeof render>) => w.find('[data-testid="hive-purge-submit"]')

beforeEach(() => {
  vi.stubGlobal(
    'ResizeObserver',
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  )
  hoisted.fetchAgent.mockRejectedValue(new Error('not found'))
})

afterEach(() => {
  vi.clearAllMocks()
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

describe('HivePurgeDialog (FR-90 P2j-3)', () => {
  it("asks for the session's title before it deletes anything", async () => {
    const w = render()
    expect(w.find('[data-testid="hive-purge-explain"]').text()).toContain('"Fix the kubelet" is deleted from every device')
    expect(submit(w).attributes('disabled'), 'nothing typed').toBeDefined()
    await w.find('[data-testid="hive-purge-confirm"] input').setValue('Fix the')
    expect(submit(w).attributes('disabled'), 'not the title').toBeDefined()
    await w.find('[data-testid="hive-purge-confirm"] input').setValue('Fix the kubelet')
    expect(submit(w).attributes('disabled'), 'the title, typed').toBeUndefined()
    expect(hoisted.purge).not.toHaveBeenCalled()
    w.unmount()

    // A session with no title is confirmed by its id.
    const untitled = render(session({ title: '' }))
    await untitled.find('[data-testid="hive-purge-confirm"] input').setValue('s1')
    expect(submit(untitled).attributes('disabled'), 'its id, for one with no title').toBeUndefined()
  })

  it('deletes it, then follows each device until every one answered', async () => {
    vi.useFakeTimers()
    hoisted.purge.mockResolvedValue({
      outcome: 'purging',
      forks: 2,
      purge: purge([
        { device_id: 'p', state: 'done', store: 'done', account: 'not_there' },
        { device_id: 'm', state: 'pending' },
      ]),
    })
    hoisted.fetchPurge.mockResolvedValue(
      purge(
        [
          { device_id: 'p', state: 'done', store: 'done', account: 'not_there' },
          { device_id: 'm', state: 'done', store: 'done', account: 'done' },
        ],
        '2026-10-16T10:00:00Z',
      ),
    )
    const w = render()
    await w.find('[data-testid="hive-purge-confirm"] input').setValue('Fix the kubelet')
    await submit(w).trigger('click')
    await flushPromises()
    expect(hoisted.purge).toHaveBeenCalledWith('t1', 's1')
    expect(w.find('[data-testid="hive-purge-status"]').text()).toContain('1 of 2 devices have answered')
    expect(w.find('[data-testid="hive-purge-forks"]').text()).toContain(': 2')
    const row = (id: string) => w.find(`[data-testid="hive-purge-device"][data-device="${id}"]`)
    expect(row('p').text(), 'each part in words').toContain("Its copy: purged · the account's part: held nothing")
    expect(row('m').text()).toContain('Big box')
    expect(row('m').text()).toContain('purged when it next connects')

    await vi.advanceTimersByTimeAsync(5000)
    await flushPromises()
    expect(hoisted.fetchPurge, 'read again while a device has not answered').toHaveBeenCalledWith('t1', 's1')
    expect(w.find('[data-testid="hive-purge-status"]').text()).toContain('Purged from every device that held it')
    await vi.advanceTimersByTimeAsync(20000)
    expect(hoisted.fetchPurge, 'not once every device answered').toHaveBeenCalledTimes(1)
  })

  it("says a refusal in the server's words, and deletes nothing", async () => {
    hoisted.purge.mockResolvedValue({ outcome: 'refused', reason: 'in_progress', message: 'the session is being moved' })
    const w = render()
    await w.find('[data-testid="hive-purge-confirm"] input').setValue('Fix the kubelet')
    await submit(w).trigger('click')
    await flushPromises()
    expect(w.find('[data-testid="hive-purge-status"]').exists(), 'no progress for a purge refused').toBe(false)
    expect(w.find('[data-testid="hive-purge-refused"]').text(), "the server's words").toContain('the session is being moved')
    expect(submit(w).exists(), 'it can be asked again').toBe(true)
  })

  it('stops reading once the dialog closes, and says why a device was closed', async () => {
    vi.useFakeTimers()
    hoisted.purge.mockResolvedValue({
      outcome: 'purging',
      purge: purge([
        { device_id: 'p', state: 'closed', closed: 'device_removed' },
        { device_id: 'q', state: 'closed', closed: 'holds_nothing' },
        { device_id: 'm', state: 'pending' },
      ]),
    })
    const w = render()
    await w.find('[data-testid="hive-purge-confirm"] input').setValue('Fix the kubelet')
    await submit(w).trigger('click')
    await flushPromises()
    expect(w.find('[data-testid="hive-purge-device"][data-device="p"]').text(), 'why a device was closed').toContain(
      'The device was removed',
    )
    expect(w.find('[data-testid="hive-purge-device"][data-device="q"]').text()).toContain('It held nothing to purge')
    await w.setProps({ modelValue: false } as unknown as Parameters<typeof w.setProps>[0])
    await vi.advanceTimersByTimeAsync(20000)
    expect(hoisted.fetchPurge, 'nothing read for a dialog closed').not.toHaveBeenCalled()
  })
})
