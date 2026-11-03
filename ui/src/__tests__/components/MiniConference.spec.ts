// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-127 P1 (review) — the mini widget reports the cameras it shows.
//
// While the call page is away, this widget is the only thing on screen
// showing remote video, and its tiles are plain `<video>`s, not VideoTiles.
// Before this it reported nothing: the call page's tiles unmounted, the store
// sent an empty view, the server paused every camera, and the widget showed
// frozen last frames while the speaker outline still moved.
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import { setActivePinia, createPinia } from 'pinia'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

vi.mock('@/composables/useCallControls', () => ({
  useCallControls: () => ({ returnToCall: vi.fn(), hangUp: vi.fn() }),
}))
vi.mock('@/stores/ws', () => ({
  useWsStore: () => ({ send: vi.fn(), onMediaMessage: vi.fn(), offMediaMessage: vi.fn(), waitForMessage: vi.fn() }),
}))

import MiniConference from '@/components/conference/MiniConference.vue'
import { useConferenceStore } from '@/stores/conference'

const vuetify = createVuetify({ components, directives })

function remote(key: string, source = 'camera') {
  const track = { kind: 'video', enabled: true, readyState: 'live', muted: false }
  const stream = {
    getVideoTracks: () => [track],
    getAudioTracks: () => [],
    getTracks: () => [track],
  } as unknown as MediaStream
  return { userId: `u-${key}`, connectionId: key, stream, kind: 'video', source }
}

beforeEach(() => {
  setActivePinia(createPinia())
})

afterEach(() => {
  vi.restoreAllMocks()
})

describe('MiniConference — the cameras it shows are reported on screen (FR-127 P1)', () => {
  it('reports its first three cameras on mount, never a screen share or a fourth', async () => {
    const store = useConferenceStore()
    store.remoteStreams.set('c1', remote('c1'))
    store.remoteStreams.set('c1:screen', remote('c1:screen', 'screen'))
    store.remoteStreams.set('c2', remote('c2'))
    store.remoteStreams.set('c3', remote('c3'))
    store.remoteStreams.set('c4', remote('c4'))
    const report = vi.spyOn(store, 'reportTileVisible')

    const w = mount(MiniConference, { global: { plugins: [vuetify] } })
    await flushPromises()

    const shown = report.mock.calls.filter(([, r]) => r.visible).map(([key]) => key)
    expect(shown.sort()).toEqual(['c1', 'c2', 'c3'])
    expect(report.mock.calls.some(([key]) => key === 'c1:screen' || key === 'c4')).toBe(false)
    expect(report.mock.calls.every(([, r]) => r.visible)).toBe(true)
    w.unmount()
  })

  it('follows the set: a camera that leaves is reported off screen, the one that takes its place on', async () => {
    const store = useConferenceStore()
    for (const k of ['c1', 'c2', 'c3', 'c4']) store.remoteStreams.set(k, remote(k))
    const report = vi.spyOn(store, 'reportTileVisible')
    const w = mount(MiniConference, { global: { plugins: [vuetify] } })
    await flushPromises()
    report.mockClear()

    store.remoteStreams.delete('c1')
    await flushPromises()

    const byKey = Object.fromEntries(report.mock.calls.map(([key, r]) => [key, r.visible]))
    expect(byKey).toEqual({ c1: false, c4: true })
    w.unmount()
  })

  it('reports every shown camera off screen when it unmounts, while the store still listens', async () => {
    const store = useConferenceStore()
    for (const k of ['c1', 'c2']) store.remoteStreams.set(k, remote(k))
    const report = vi.spyOn(store, 'reportTileVisible')
    const w = mount(MiniConference, { global: { plugins: [vuetify] } })
    await flushPromises()
    report.mockClear()

    w.unmount()
    const byKey = Object.fromEntries(report.mock.calls.map(([key, r]) => [key, r.visible]))
    expect(byKey).toEqual({ c1: false, c2: false })
  })

  it("the store's per-key count tolerates the widget's report overlapping a tile's during the page switch", () => {
    // A tile reports on, the widget reports on (two reporters), the tile
    // unmounts (off): the key stays on screen until the widget goes too.
    const store = useConferenceStore()
    store.reportTileVisible('c1', { visible: true, w: 320, h: 240 })
    store.reportTileVisible('c1', { visible: true, w: 96, h: 72 })
    store.reportTileVisible('c1', { visible: false, w: 0, h: 0 })
    expect(store.isTileOnScreen('c1')).toBe(true)
    store.reportTileVisible('c1', { visible: false, w: 0, h: 0 })
    expect(store.isTileOnScreen('c1')).toBe(false)
  })
})
