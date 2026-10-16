// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-90 P2i-3 — the search box on Agent sessions. Shown only where the
// organization's copies can be searched, asked again at every visit; a query
// is asked once typing pauses (Enter at once); a hit's marks are drawn as
// <mark> from text, so a snippet that looks like HTML stays text; a hit names
// its session by the title the list holds, else says it is one you can read;
// a click opens its room; a refusal is said in words, a word this build does
// not know as itself.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import type { Ref, ShallowRef } from 'vue'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'

/** The search's state, set by hand: made in the mock's factory, which runs
 *  before anything at the top level of this file does. */
interface SearchState {
  status: Ref<string>
  reason: Ref<string | null>
  detail: Ref<string | null>
  named: Ref<number | null>
  searched: Ref<number | null>
  hits: ShallowRef<Array<{ session: string; seq: number; snippet: string }>>
  answered: Ref<string | null>
  searching: Ref<boolean>
  error: Ref<string | null>
}

const hoisted = vi.hoisted(() => ({
  fetchSessions: vi.fn(),
  fetchSession: vi.fn(),
  checkSearchable: vi.fn(),
  push: vi.fn(),
  run: vi.fn(),
  close: vi.fn(),
  hive: {
    sessions: [] as Array<{ id: string; title: string; room_id?: string; status: string }>,
    loading: false,
    searchable: {} as Record<string, boolean>,
  },
  state: null as SearchState | null,
}))

vi.mock('@/stores/hive', () => ({
  isAdopted: () => false,
  isLive: () => false,
  useHiveStore: () => ({
    get sessions() {
      return hoisted.hive.sessions
    },
    get loading() {
      return hoisted.hive.loading
    },
    get searchable() {
      return hoisted.hive.searchable
    },
    fetchSessions: hoisted.fetchSessions,
    fetchSession: hoisted.fetchSession,
    checkSearchable: hoisted.checkSearchable,
  }),
}))
vi.mock('@/stores/devices', () => ({
  useDeviceStore: () => ({ items: [], loading: false, fetchDevices: vi.fn() }),
}))
vi.mock('vue-router', () => ({
  useRoute: () => ({ params: { tenantId: 't1' } }),
  useRouter: () => ({ push: hoisted.push }),
}))
vi.mock('@/stores/ws', () => ({ useWsStore: () => ({}) }))

// The search itself is `useHiveSearch`'s (its own spec); here its state is
// set by hand. `snippetParts` and the limit stay the real ones.
vi.mock('@/composables/useHiveSearch', async (importOriginal) => {
  const real = await importOriginal<typeof import('@/composables/useHiveSearch')>()
  const { ref, shallowRef } = await import('vue')
  hoisted.state = {
    status: ref('idle'),
    reason: ref(null),
    detail: ref(null),
    named: ref(null),
    searched: ref(null),
    hits: shallowRef([]),
    answered: ref(null),
    searching: ref(false),
    error: ref(null),
  }
  return {
    ...real,
    useHiveSearch: () => ({ ...hoisted.state!, run: hoisted.run, close: hoisted.close }),
  }
})

import HiveSessionsView from '@/views/hive/HiveSessionsView.vue'

const state = (): SearchState => hoisted.state!

const vuetify = createVuetify({ components, directives })

async function render() {
  const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
  const w = mount(HiveSessionsView, { global: { plugins: [vuetify, i18n] } })
  await flushPromises()
  return w
}

beforeEach(() => {
  vi.stubGlobal(
    'ResizeObserver',
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  )
  hoisted.hive.sessions = [{ id: 's-mine', title: 'Fix the kubelet', room_id: 'r-mine', status: 'ended' }]
  hoisted.hive.searchable = { t1: true }
  hoisted.checkSearchable.mockResolvedValue(true)
  state().status.value = 'idle'
  state().reason.value = null
  state().detail.value = null
  state().searched.value = null
  state().hits.value = []
  state().answered.value = null
  state().searching.value = false
  state().error.value = null
})

afterEach(() => {
  vi.clearAllMocks()
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

async function type(w: Awaited<ReturnType<typeof render>>, text: string): Promise<void> {
  await w.find('[data-testid="hive-search"] input').setValue(text)
}

describe('HiveSessionsView — the search box (FR-90 P2i-3)', () => {
  it('asks at every visit whether the copies can be searched, and shows the box only then', async () => {
    hoisted.hive.searchable = {}
    let w = await render()
    expect(hoisted.checkSearchable, 'asked at every visit').toHaveBeenCalledWith('t1')
    expect(w.find('[data-testid="hive-search"]').exists(), 'no box while it cannot be searched').toBe(false)
    w.unmount()

    hoisted.hive.searchable = { t1: false }
    w = await render()
    expect(w.find('[data-testid="hive-search"]').exists()).toBe(false)
    w.unmount()

    hoisted.hive.searchable = { t1: true }
    w = await render()
    expect(w.find('[data-testid="hive-search"]').exists()).toBe(true)
    // The list is still there until something is typed.
    expect(w.find('[data-testid="hive-session-s-mine"]').exists()).toBe(true)
    w.unmount()
  })

  it('asks once typing pauses, Enter at once, and clearing asks for nothing', async () => {
    vi.useFakeTimers()
    const w = await render()
    await type(w, 'kub')
    await type(w, 'kubelet')
    expect(hoisted.run, 'nothing asked while typing').not.toHaveBeenCalled()
    await vi.advanceTimersByTimeAsync(300)
    expect(hoisted.run).toHaveBeenCalledTimes(1)
    expect(hoisted.run).toHaveBeenLastCalledWith('t1', 'kubelet')
    // The results take the list's place while a query stands.
    expect(w.find('[data-testid="hive-search-results"]').exists()).toBe(true)
    expect(w.find('[data-testid="hive-session-s-mine"]').exists()).toBe(false)

    await type(w, 'etcd')
    await w.find('[data-testid="hive-search"] input').trigger('keydown', { key: 'Enter' })
    expect(hoisted.run).toHaveBeenLastCalledWith('t1', 'etcd')
    const asked = hoisted.run.mock.calls.length
    await vi.advanceTimersByTimeAsync(300)
    expect(hoisted.run.mock.calls.length, 'the pause asks nothing more').toBe(asked)

    await type(w, '')
    expect(hoisted.run).toHaveBeenLastCalledWith('t1', '')
    expect(w.find('[data-testid="hive-search-results"]').exists()).toBe(false)
    expect(w.find('[data-testid="hive-session-s-mine"]').exists()).toBe(true)
    w.unmount()
  })

  it("draws the device's marks as <mark> from text: a snippet that looks like HTML stays text", async () => {
    const w = await render()
    await type(w, 'kubelet')
    state().hits.value = [
      { session: 's-mine', seq: 4, snippet: '…the [kubelet] said <img src=x onerror="alert(1)"> [twice]' },
      { session: 's-theirs', seq: 9, snippet: '[kubelet] ok' },
    ]
    state().answered.value = 'kubelet'
    state().searched.value = 12
    await flushPromises()

    const first = w.find('[data-testid="hive-hit-0"]')
    expect(first.find('img').exists(), 'never parsed as HTML').toBe(false)
    expect(first.text()).toContain('<img src=x onerror="alert(1)">')
    expect(first.findAll('mark').map((m) => m.text())).toEqual(['kubelet', 'twice'])
    // A session of yours by its title; one read through a room as such.
    expect(first.text(), 'your session by its title').toContain('Fix the kubelet')
    expect(w.find('[data-testid="hive-hit-1"]').text()).toContain('A session you can read')
    expect(w.find('[data-testid="hive-search-scope"]').text()).toContain('Sessions searched: 12')
    expect(w.find('[data-testid="hive-search-none"]').exists()).toBe(false)
    w.unmount()
  })

  it('says nothing was found only for the query asked', async () => {
    const w = await render()
    await type(w, 'nothing')
    state().answered.value = 'noth'
    await flushPromises()
    expect(w.find('[data-testid="hive-search-none"]').exists(), 'not for another query').toBe(false)
    state().answered.value = 'nothing'
    await flushPromises()
    expect(w.find('[data-testid="hive-search-none"]').text()).toContain('Nothing found')
    w.unmount()
  })

  it("opens a hit's room: a session of yours from the list, another read first", async () => {
    const w = await render()
    await type(w, 'kubelet')
    state().hits.value = [
      { session: 's-mine', seq: 4, snippet: '[kubelet]' },
      { session: 's-theirs', seq: 9, snippet: '[kubelet]' },
    ]
    await flushPromises()

    await w.find('[data-testid="hive-hit-0"]').trigger('click')
    await flushPromises()
    expect(hoisted.fetchSession).not.toHaveBeenCalled()
    expect(hoisted.push).toHaveBeenLastCalledWith('/tenant/t1/room/r-mine')

    hoisted.fetchSession.mockResolvedValue({ id: 's-theirs', title: 'Theirs', room_id: 'r-theirs', status: 'idle' })
    await w.find('[data-testid="hive-hit-1"]').trigger('click')
    await flushPromises()
    expect(hoisted.fetchSession, "another's session is read first").toHaveBeenCalledWith('t1', 's-theirs')
    expect(hoisted.push).toHaveBeenLastCalledWith('/tenant/t1/room/r-theirs')

    // One no longer readable says so, and goes nowhere.
    hoisted.push.mockClear()
    hoisted.fetchSession.mockRejectedValue(new Error('not found'))
    await w.find('[data-testid="hive-hit-1"]').trigger('click')
    await flushPromises()
    expect(hoisted.push).not.toHaveBeenCalled()
    expect(w.find('[data-testid="hive-search-results"]').text()).toContain('Could not open that session: not found')
    w.unmount()
  })

  it('says a refusal in words, a word it does not know as itself, with its detail', async () => {
    const w = await render()
    await type(w, 'kubelet')
    state().status.value = 'refused'
    state().reason.value = 'no_archive_replica'
    await flushPromises()
    expect(w.find('[data-testid="hive-search-refused"]').text()).toContain(
      'No archive replica that searches is online right now.',
    )

    state().reason.value = 'hive_disabled'
    state().detail.value = 'this device serves a search only as an archive replica (hive_archive)'
    await flushPromises()
    expect(w.find('[data-testid="hive-search-refused"]').text()).toContain(
      'does not serve searches. (this device serves a search only as an archive replica (hive_archive))',
    )

    state().reason.value = 'a_newer_word'
    state().detail.value = null
    await flushPromises()
    expect(w.find('[data-testid="hive-search-refused"]').text(), 'a word it does not know, as itself').toContain(
      'The search was refused: a_newer_word',
    )

    // A failed search, in the device's words.
    state().status.value = 'open'
    state().error.value = 'this device could not search its copies'
    await flushPromises()
    expect(w.find('[data-testid="hive-search-refused"]').exists()).toBe(false)
    expect(w.find('[data-testid="hive-search-error"]').text()).toContain('this device could not search its copies')
    w.unmount()
  })
})
