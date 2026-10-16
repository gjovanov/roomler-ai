// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-90 P2j-1 — a session's copies, from its room. Shown only for a record
// that carries a replicaset; each copy by its device's name, its role and
// state, and how far behind the primary's it is, by the server's own count;
// the owner's Move, the copy that holds the most preselected, the folder sent
// only when changed, the answer said in the server's words; anyone else sees
// the copies and no Move; and why it cannot move, when it cannot.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'
import type { HiveReplicaMember, HiveSession } from '@/stores/hive'

const hoisted = vi.hoisted(() => ({
  sessions: [] as HiveSession[],
  fetchSession: vi.fn(),
  promote: vi.fn(),
  fetchAgent: vi.fn(),
  agents: null as null | Array<{ id: string; name: string; display_name?: string }>,
  user: { id: 'u-owner' } as { id: string } | null,
}))
vi.mock('@/stores/hive', async (importOriginal) => {
  const real = await importOriginal<typeof import('@/stores/hive')>()
  return {
    ...real,
    useHiveStore: () => ({
      get sessions() {
        return hoisted.sessions
      },
      fetchSession: hoisted.fetchSession,
      promote: hoisted.promote,
    }),
  }
})
vi.mock('@/stores/agents', async () => {
  const { reactive } = await import('vue')
  hoisted.agents = reactive([])
  return { useAgentStore: () => ({ agents: hoisted.agents, fetchAgent: hoisted.fetchAgent }) }
})
vi.mock('@/stores/auth', () => ({
  useAuthStore: () => ({
    get user() {
      return hoisted.user
    },
  }),
}))

import HiveCopies from '@/components/hive/HiveCopies.vue'

const vuetify = createVuetify({ components, directives })

/** The dialog inline: its activator opens it, its content is always there. */
const DialogStub = {
  props: ['modelValue'],
  emits: ['update:modelValue'],
  template:
    '<div><slot name="activator" :props="{ onClick: () => $emit(\'update:modelValue\', true) }" /><slot /></div>',
}

const member = (device_id: string, role: string, state: string, holds?: number, refusal?: string): HiveReplicaMember => ({
  device_id,
  role,
  state,
  ...(refusal ? { refusal } : {}),
  ...(holds === undefined ? {} : { holds, tip: { fence: 2, seq: holds, hash: 'ab', at: '' } }),
})

function record(over: Partial<HiveSession> = {}, members?: HiveReplicaMember[]): HiveSession {
  return {
    id: 's1',
    owner_id: 'u-owner',
    title: 'Fix the kubelet',
    harness: 'claude-code',
    harness_session: 'h',
    device_id: 'p',
    device_name: 'laptop',
    folder: '/work/app',
    status: 'running',
    fence: 2,
    created_at: '',
    updated_at: '',
    replicaset: {
      policy_revision: 1,
      min: 2,
      max: 4,
      restricted_tags: [],
      members: members ?? [
        member('p', 'primary', 'joined', 40),
        member('a', 'owner', 'joined', 31),
        member('d', 'owner', 'joined', 38),
        member('arch', 'archive', 'joined', 40),
        member('b', 'owner', 'pending'),
        member('c', 'owner', 'refused', undefined, 'not_replica'),
      ],
      placed_at: '',
    },
    ...over,
  }
}

async function render(s: HiveSession, open = true) {
  hoisted.sessions = [s]
  hoisted.fetchSession.mockResolvedValue(s)
  const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
  const w = mount(HiveCopies, {
    props: { tenantId: 't1', sessionId: 's1' },
    global: { plugins: [vuetify, i18n], stubs: { VDialog: DialogStub } },
  })
  if (open && w.find('[data-testid="hive-copies-open"]').exists()) {
    await w.find('[data-testid="hive-copies-open"]').trigger('click')
    await flushPromises()
  }
  return w
}

const copy = (w: Awaited<ReturnType<typeof render>>, id: string) => w.find(`[data-testid="hive-copy"][data-device="${id}"]`)
/** Why it cannot move, or '' when the panel offers a move. */
const notMovable = (w: Awaited<ReturnType<typeof render>>) => {
  const p = w.find('[data-testid="hive-copies-not-movable"]')
  return p.exists() ? p.text() : ''
}

beforeEach(() => {
  // jsdom has no ResizeObserver; a button's spinner needs one.
  vi.stubGlobal(
    'ResizeObserver',
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  )
  hoisted.user = { id: 'u-owner' }
  hoisted.agents!.splice(0, hoisted.agents!.length, { id: 'p', name: 'laptop' }, { id: 'a', name: 'desktop' }, {
    id: 'd',
    name: 'workstation',
    display_name: 'Big box',
  }, { id: 'arch', name: 'archive-1' })
  hoisted.fetchAgent.mockRejectedValue(new Error('not found'))
})

afterEach(() => {
  vi.clearAllMocks()
  vi.unstubAllGlobals()
})

describe('HiveCopies (FR-90 P2j-1)', () => {
  it('shows nothing for a record placed nowhere', async () => {
    const w = await render(record({ replicaset: undefined }), false)
    expect(w.find('[data-testid="hive-copies-open"]').exists(), 'no copies, no button').toBe(false)
  })

  it('lists each copy by its name, role and state, and how far behind the primary it is', async () => {
    const w = await render(record())
    expect(hoisted.fetchSession).toHaveBeenCalledWith('t1', 's1')
    expect(copy(w, 'p').text()).toContain('laptop')
    expect(copy(w, 'p').text()).toContain('Runs it · 40 events')
    expect(copy(w, 'a').text(), "the server's count, against the primary's").toContain('9 events behind · 31 events')
    expect(copy(w, 'd').text()).toContain('Big box')
    expect(copy(w, 'arch').text()).toContain('Up to date · 40 events')
    expect(copy(w, 'arch').text()).toContain('Archive replica')
    expect(copy(w, 'b').text()).toContain('No copy reported yet')
    expect(copy(w, 'c').text()).toContain('Refused: not_replica')
    // A device the page has not seen is read, once each; one it cannot read is named generically.
    expect(hoisted.fetchAgent.mock.calls.map((c) => c[1]).sort(), 'each device the page has not seen is read').toEqual([
      'b',
      'c',
    ])
    expect(copy(w, 'b').text()).toContain('A device of your organization')
  })

  it('gives the owner Move, with the copy that holds the most preselected, and anyone else none', async () => {
    let w = await render(record())
    expect(w.find('[data-testid="hive-copies-move"]').exists()).toBe(true)
    const targets = w.findAll('[data-testid^="hive-copies-target-"]').map((t) => t.attributes('data-testid'))
    expect(targets, 'joined copies on devices that run sessions').toEqual(['hive-copies-target-a', 'hive-copies-target-d'])
    expect((w.find('[data-testid="hive-copies-target-d"] input').element as HTMLInputElement).checked, 'the copy that holds the most').toBe(true)
    expect((w.find('[data-testid="hive-copies-folder"] input').element as HTMLInputElement).value).toBe('/work/app')
    w.unmount()

    hoisted.user = { id: 'u-reader' }
    w = await render(record())
    expect(w.find('[data-testid="hive-copies-move"]').exists(), 'only the owner moves it').toBe(false)
    expect(copy(w, 'a').exists()).toBe(true)
  })

  it('moves to the chosen copy, the folder only when changed, and says how it went', async () => {
    const w = await render(record())
    const moved = record({ promotion: { id: 'm1', to: 'd', to_name: 'Big box', folder: '/work/app', from: 'p', from_fence: 2, by: 'u-owner', state: 'draining', started_at: '', deadline: '', teleport: false, fork: false } })
    hoisted.promote.mockResolvedValueOnce({ outcome: 'promoting', session: moved })
    await w.find('[data-testid="hive-copies-move-submit"]').trigger('click')
    await flushPromises()
    expect(hoisted.promote, 'the folder only when it changed').toHaveBeenLastCalledWith('t1', 's1', { device_id: 'd' })
    expect(w.find('[data-testid="hive-copies-outcome"]').text()).toContain('Moving: the device is ready')
    expect(w.find('[data-testid="hive-copies-moving"]').text(), 'the answer is the record shown').toContain(
      'Moving to Big box: the device it runs on is finishing its turn.',
    )
    w.unmount()

    const w2 = await render(record())
    await w2.find('[data-testid="hive-copies-target-a"] input').setValue(true)
    await w2.find('[data-testid="hive-copies-folder"] input').setValue('/work/other')
    hoisted.promote.mockResolvedValueOnce({
      outcome: 'refused',
      reason: 'not_freshest',
      message: 'Big box holds more of the session than desktop',
      session: record(),
    })
    await w2.find('[data-testid="hive-copies-move-submit"]').trigger('click')
    await flushPromises()
    expect(hoisted.promote, 'the folder, since it changed').toHaveBeenLastCalledWith('t1', 's1', {
      device_id: 'a',
      folder: '/work/other',
    })
    expect(w2.find('[data-testid="hive-copies-outcome"]').text(), "the server's words").toContain(
      'Big box holds more of the session than desktop',
    )
  })

  it('says why it cannot move: being moved already, not running, adopted', async () => {
    const promotion = { id: 'm1', to: 'd', to_name: '', folder: '/work/app', from: 'p', from_fence: 2, by: 'u-owner', state: 'syncing', started_at: '', deadline: '', teleport: false, fork: false }
    let w = await render(record({ promotion }))
    expect(w.find('[data-testid="hive-copies-moving"]').text(), 'named from the page when the record has no name').toContain(
      'Moving to Big box: the copy is catching up.',
    )
    expect(notMovable(w), 'not while it moves').toContain('It is being moved already.')
    expect(w.find('[data-testid="hive-copies-move-submit"]').exists()).toBe(false)
    w.unmount()

    w = await render(record({ status: 'ended' }))
    expect(notMovable(w), 'not once it ended').toContain('Only a running session moves.')
    w.unmount()

    w = await render(record({ origin: 'adopted' }))
    expect(notMovable(w), 'an adopted session stays').toContain('stays where it runs')
  })

  it('warns when a copy that is no target holds more than every target', async () => {
    const w = await render(
      record({}, [member('p', 'primary', 'joined', 40), member('a', 'owner', 'joined', 31), member('arch', 'archive', 'joined', 40)]),
    )
    expect(w.find('[data-testid="hive-copies-ahead"]').text(), 'the server would refuse every move').toContain(
      'archive-1 holds more of this session than any device it can move to',
    )
  })
})
