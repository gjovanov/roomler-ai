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
  teleport: vi.fn(),
  fork: vi.fn(),
  fetchAgent: vi.fn(),
  fetchAgents: vi.fn(),
  agents: null as null | Array<{
    id: string
    name: string
    display_name?: string
    presence?: 'online' | 'stale' | 'offline'
    is_online?: boolean
    capabilities?: { rpc?: string[] }
  }>,
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
      teleport: hoisted.teleport,
      fork: hoisted.fork,
    }),
  }
})
vi.mock('@/stores/agents', async () => {
  const { reactive } = await import('vue')
  hoisted.agents = reactive([])
  return {
    useAgentStore: () => ({ agents: hoisted.agents, fetchAgent: hoisted.fetchAgent, fetchAgents: hoisted.fetchAgents }),
  }
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
/** Why it cannot be forked, or '' when the panel offers a fork. */
const notForkable = (w: Awaited<ReturnType<typeof render>>) => {
  const p = w.find('[data-testid="hive-copies-not-forkable"]')
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

// FR-90 P2j-2 — the owner's teleport and fork, from the same panel: the
// organization's devices are read once either is opened.
describe('HiveCopies — move elsewhere and fork (FR-90 P2j-2)', () => {
  const online = (id: string, name: string, rpc: string[], display_name?: string) => ({
    id,
    name,
    ...(display_name ? { display_name } : {}),
    presence: 'online' as const,
    is_online: true,
    capabilities: { rpc },
  })
  /** The organization's devices, as `fetchAgents` reads them. */
  function fleet(): void {
    hoisted.fetchAgents.mockImplementation(async () => {
      hoisted.agents!.splice(
        0,
        hoisted.agents!.length,
        online('p', 'laptop', ['hive', 'hive-teleport', 'hive-fork']),
        online('a', 'desktop', ['hive', 'hive-teleport', 'hive-fork']),
        online('d', 'workstation', ['hive', 'hive-teleport', 'hive-fork'], 'Big box'),
        online('arch', 'archive-1', ['hive', 'hive-teleport', 'hive-fork', 'hive-archive']),
        online('n', 'new-mac', ['hive', 'hive-teleport', 'hive-fork']),
      )
    })
  }

  async function open(w: Awaited<ReturnType<typeof render>>, action: 'teleport' | 'fork'): Promise<void> {
    await w.find(`[data-testid="hive-copies-action-${action}"]`).trigger('click')
    await flushPromises()
  }

  it('moves it to a device that holds no copy, from the devices the organization has', async () => {
    fleet()
    const w = await render(record())
    await open(w, 'teleport')
    expect(hoisted.fetchAgents, "the organization's devices, read once").toHaveBeenCalledTimes(1)
    expect(
      w.findAll('[data-testid^="hive-copies-teleport-to-"]').map((x) => x.attributes('data-testid')),
      'a device with no copy taken, not where it runs, no archive replica',
    ).toEqual(['hive-copies-teleport-to-n'])
    expect(
      (w.find('[data-testid="hive-copies-teleport-to-n"] input').element as HTMLInputElement).checked,
      'the one offered, picked',
    ).toBe(true)
    hoisted.teleport.mockResolvedValueOnce({ outcome: 'teleporting', session: record() })
    await w.find('[data-testid="hive-copies-teleport-submit"]').trigger('click')
    await flushPromises()
    expect(hoisted.teleport, 'to the device picked, the folder only when changed').toHaveBeenLastCalledWith('t1', 's1', {
      device_id: 'n',
    })
    expect(w.find('[data-testid="hive-copies-outcome"]').text()).toContain('the device is taking its copy')

    hoisted.teleport.mockResolvedValueOnce({
      outcome: 'refused',
      reason: 'primary_offline',
      message: 'the device it runs on is offline: a teleport copies from it',
      session: record(),
    })
    await w.find('[data-testid="hive-copies-teleport-submit"]').trigger('click')
    await flushPromises()
    expect(w.find('[data-testid="hive-copies-outcome"]').text(), "a refusal in the server's words").toContain(
      'a teleport copies from it',
    )
    await open(w, 'fork')
    expect(hoisted.fetchAgents, 'not read again').toHaveBeenCalledTimes(1)
  })

  it('forks it on a device that holds a copy or takes one, in a folder of its own', async () => {
    fleet()
    const w = await render(record())
    await open(w, 'fork')
    const labels = w.findAll('[data-testid^="hive-copies-fork-on-"]')
    expect(labels.map((x) => x.text()), 'its copies first, then a device that takes one, no archive replica').toEqual([
      'laptop · from its copy',
      'desktop · from its copy',
      'Big box · from its copy',
      'new-mac · takes a copy first',
    ])
    const submit = () => w.find('[data-testid="hive-copies-fork-submit"]')
    expect(submit().attributes('disabled'), 'a folder of its own first').toBeDefined()
    await w.find('[data-testid="hive-copies-fork-on-n"] input').setValue(true)
    await w.find('[data-testid="hive-copies-fork-folder"] input').setValue('/work/try')
    hoisted.fork.mockResolvedValueOnce({ outcome: 'joining', session: { ...record(), id: 'f1', room_id: 'r-fork' } })
    await submit().trigger('click')
    await flushPromises()
    expect(hoisted.fork, 'no name sent unless typed').toHaveBeenLastCalledWith('t1', 's1', {
      device_id: 'n',
      folder: '/work/try',
    })
    expect(w.find('[data-testid="hive-copies-outcome"]').text()).toContain('the device is taking a copy first')
    expect(w.find('[data-testid="hive-copies-fork-room"]').exists(), "the fork's own room").toBe(true)

    await w.find('[data-testid="hive-copies-fork-title"] input').setValue('Try another way')
    hoisted.fork.mockResolvedValueOnce({ outcome: 'refused', reason: 'same_folder', message: 'the session it forks runs in this folder' })
    await submit().trigger('click')
    await flushPromises()
    expect(hoisted.fork).toHaveBeenLastCalledWith('t1', 's1', { device_id: 'n', folder: '/work/try', title: 'Try another way' })
    expect(w.find('[data-testid="hive-copies-outcome"]').text(), "the device's refusal in its words").toContain(
      'the session it forks runs in this folder',
    )
    expect(w.find('[data-testid="hive-copies-fork-room"]').exists(), 'no room for a fork refused').toBe(false)
  })

  it('says why it cannot fork: adopted from a terminal, being moved', async () => {
    fleet()
    let w = await render(record({ origin: 'adopted' }))
    await open(w, 'fork')
    expect(notForkable(w), 'an adopted session is not forked').toContain('is not forked')
    w.unmount()
    const promotion = { id: 'm1', to: 'd', to_name: 'Big box', folder: '/work/app', from: 'p', from_fence: 2, by: 'u-owner', state: 'draining', started_at: '', deadline: '', teleport: false, fork: false }
    w = await render(record({ promotion }))
    await open(w, 'fork')
    expect(notForkable(w), 'not while it moves').toContain('once it settles')
  })
})
