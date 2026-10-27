// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-25 — every VideoTile a layout renders MUST receive `stream-key`.
//
// Picture-in-picture resolves its target with
// `document.querySelector('video[data-stream-key="…"]')`, so a tile rendered
// without the prop is invisible to it. That was the original "PiP does
// nothing" bug (no layout passed it at all), and the first fix missed the
// FLOATING self-view — which is the ONLY tile on screen in a solo call, so PiP
// still did nothing there, now with a "That video is not on screen right now"
// snackbar instead of silence. Field-caught on prod 2026-08-29.
//
// A prop asserted in review is a prop that goes missing again; this asserts it.
import { describe, it, expect, beforeAll, vi } from 'vitest'
import { mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

import TiledLayout from '@/components/conference/layouts/TiledLayout.vue'
import SpotlightLayout from '@/components/conference/layouts/SpotlightLayout.vue'
import SidebarLayout from '@/components/conference/layouts/SidebarLayout.vue'
import type { LayoutParticipant } from '@/composables/useConferenceLayout'

beforeAll(() => {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver
  if (!globalThis.requestAnimationFrame) {
    globalThis.requestAnimationFrame = vi.fn((cb: FrameRequestCallback) => {
      cb(0)
      return 0
    }) as unknown as typeof requestAnimationFrame
  }
})

const vuetify = createVuetify({ components, directives })

function participant(over: Partial<LayoutParticipant> = {}): LayoutParticipant {
  return {
    streamKey: 'user-1:cam',
    userId: 'user-1',
    displayName: 'Someone',
    stream: null,
    isMuted: false,
    isLocal: false,
    isScreenShare: false,
    isPinned: false,
    videoPaused: false,
    audioLevel: 0,
    ...over,
  }
}

const self = participant({ streamKey: 'me:cam', userId: 'me', displayName: 'Me', isLocal: true })
const remote = participant({ streamKey: 'them:cam', userId: 'them', displayName: 'Them' })

// Stub VideoTile so the assertion is about what the LAYOUT passes, not about
// what the tile chooses to render.
const stubs = {
  VideoTile: {
    props: ['streamKey', 'stream', 'displayName', 'isMuted', 'isLocal', 'compact', 'objectFit'],
    template: '<div class="tile-stub" :data-stream-key="streamKey" />',
  },
}

const cases = [
  {
    name: 'TiledLayout',
    component: TiledLayout,
    props: {
      participants: [remote],
      selfParticipant: self,
      selfViewFloating: true,
      selfViewMode: 'floating' as const,
      activeSpeakerKey: null,
    },
  },
  {
    name: 'SpotlightLayout',
    component: SpotlightLayout,
    props: {
      primary: [remote],
      secondary: [],
      selfParticipant: self,
      selfViewFloating: true,
      selfViewMode: 'floating' as const,
      activeSpeakerKey: null,
    },
  },
  {
    name: 'SidebarLayout',
    component: SidebarLayout,
    props: {
      primary: [remote],
      secondary: [],
      selfParticipant: self,
      selfViewFloating: true,
      selfViewMode: 'floating' as const,
      activeSpeakerKey: null,
    },
  },
]

describe('conference layouts — every tile carries a stream key', () => {
  for (const c of cases) {
    it(`${c.name} passes stream-key to every VideoTile, floating self-view included`, () => {
      const wrapper = mount(c.component, {
        props: c.props as never,
        global: { plugins: [vuetify], stubs },
      })

      const tiles = wrapper.findAll('.tile-stub')
      expect(tiles.length, 'no tiles rendered — the fixture is wrong').toBeGreaterThan(0)

      for (const tile of tiles) {
        expect(
          tile.attributes('data-stream-key'),
          'a VideoTile was rendered without stream-key; PiP cannot find it',
        ).toBeTruthy()
      }

      // The floating self-view is the one that regressed, and in a solo call it
      // is the ONLY video on the page — so name it explicitly.
      const keys = tiles.map((t) => t.attributes('data-stream-key'))
      expect(keys).toContain('me:cam')
    })
  }
})

// FR-124 — a tile's `picture-black` reaches the call page WITH its stream key,
// from every tile a layout renders. The floating self-view is named again: it
// is the tile that shows your own (covered) camera in a solo call.
const forwardingStub = {
  props: ['streamKey', 'isScreenShare'],
  emits: ['picture-black'],
  template: '<div class="tile-stub" :data-stream-key="streamKey" :data-screen="isScreenShare ? 1 : 0" />',
}

describe('conference layouts — a black picture is reported with its stream key (FR-124)', () => {
  for (const c of cases) {
    it(`${c.name} forwards picture-black from every tile, floating self-view included`, async () => {
      const wrapper = mount(c.component, {
        props: c.props as never,
        global: { plugins: [vuetify], stubs: { VideoTile: forwardingStub } },
      })
      const tiles = wrapper.findAllComponents(forwardingStub)
      expect(tiles.length).toBeGreaterThan(0)
      for (const tile of tiles) tile.vm.$emit('picture-black', true)

      const reported = (wrapper.emitted('picture-black') ?? []) as [string, boolean][]
      expect(reported).toHaveLength(tiles.length)
      expect(reported.map(([key]) => key).sort()).toEqual(
        tiles.map((t) => t.props('streamKey') as string).sort(),
      )
      expect(reported).toContainEqual(['me:cam', true])
    })
  }

  it('a shared screen is marked as one, so its picture is never judged', () => {
    const screen = participant({ streamKey: 'them:screen', isScreenShare: true })
    const wrapper = mount(SpotlightLayout, {
      props: {
        primary: [screen],
        secondary: [remote],
        selfParticipant: null,
        selfViewFloating: false,
        selfViewMode: 'in-grid-cropped',
        activeSpeakerKey: null,
      },
      global: { plugins: [vuetify], stubs: { VideoTile: forwardingStub } },
    })
    const byKey = Object.fromEntries(
      wrapper.findAll('.tile-stub').map((t) => [t.attributes('data-stream-key'), t.attributes('data-screen')]),
    )
    expect(byKey).toEqual({ 'them:screen': '1', 'them:cam': '0' })
  })
})

// FR-127 P1 — a tile's `visible` report reaches the call page WITH its stream
// key from every remote tile a layout renders: the grid, the big tile, and
// the strip. The floating self-view is local and never reports, so no layout
// listens to it for this.
const visibleStub = {
  props: ['streamKey', 'isLocal'],
  emits: ['visible'],
  template: '<div class="tile-stub" :data-stream-key="streamKey" />',
}
const remote2 = participant({ streamKey: 'other:cam', userId: 'other', displayName: 'Other' })

describe('conference layouts — a tile on screen is reported with its stream key (FR-127 P1)', () => {
  const withStrip = [
    { name: 'TiledLayout', component: TiledLayout, props: { ...cases[0].props, participants: [remote, remote2] } },
    { name: 'SpotlightLayout', component: SpotlightLayout, props: { ...cases[1].props, secondary: [remote2] } },
    { name: 'SidebarLayout', component: SidebarLayout, props: { ...cases[2].props, secondary: [remote2] } },
  ]
  for (const c of withStrip) {
    it(`${c.name} forwards visible from every remote tile, strip included, never the floating self-view`, async () => {
      const wrapper = mount(c.component, {
        props: c.props as never,
        global: { plugins: [vuetify], stubs: { VideoTile: visibleStub } },
      })
      const tiles = wrapper.findAllComponents(visibleStub)
      expect(tiles.length).toBe(3)
      const report = { visible: true, w: 320, h: 240 }
      for (const tile of tiles) tile.vm.$emit('visible', report)

      const reported = (wrapper.emitted('visible') ?? []) as [string, typeof report][]
      expect(reported.map(([key]) => key).sort()).toEqual(['other:cam', 'them:cam'])
      expect(reported.every(([, r]) => r.visible === report.visible && r.w === report.w && r.h === report.h)).toBe(true)
    })
  }
})

// FR-127 P2 — the layout, not the tile, knows which tile is spotlit: the
// big tile of spotlight and sidebar, and a pinned grid tile. Its report
// carries `spotlit: true`; a strip tile's and a plain grid tile's carry
// nothing, so an older server reads past the field.
describe('conference layouts — the spotlit tile is marked in its report (FR-127 P2)', () => {
  const report = { visible: true, w: 320, h: 240 }
  const reportsByKey = (wrapper: ReturnType<typeof mount>) => {
    const out = new Map<string, Record<string, unknown>>()
    for (const [key, r] of (wrapper.emitted('visible') ?? []) as [string, Record<string, unknown>][]) {
      out.set(key, r)
    }
    return out
  }
  const emitAll = (wrapper: ReturnType<typeof mount>) => {
    for (const tile of wrapper.findAllComponents(visibleStub)) tile.vm.$emit('visible', report)
  }

  for (const c of [
    { name: 'SpotlightLayout', component: SpotlightLayout, props: { ...cases[1].props, secondary: [remote2] } },
    { name: 'SidebarLayout', component: SidebarLayout, props: { ...cases[2].props, secondary: [remote2] } },
  ]) {
    it(`${c.name} marks the big tile spotlit and the strip tile not`, () => {
      const wrapper = mount(c.component, {
        props: c.props as never,
        global: { plugins: [vuetify], stubs: { VideoTile: visibleStub } },
      })
      emitAll(wrapper)
      const byKey = reportsByKey(wrapper)
      expect(byKey.get('them:cam')).toEqual({ ...report, spotlit: true })
      expect(byKey.get('other:cam')).toEqual(report)
      expect(byKey.get('other:cam')).not.toHaveProperty('spotlit')
    })
  }

  it('TiledLayout marks a pinned tile spotlit and a plain one not', () => {
    const pinned = participant({ streamKey: 'pinned:cam', userId: 'pinned', displayName: 'Pinned', isPinned: true })
    const wrapper = mount(TiledLayout, {
      props: { ...cases[0].props, participants: [remote, pinned] } as never,
      global: { plugins: [vuetify], stubs: { VideoTile: visibleStub } },
    })
    emitAll(wrapper)
    const byKey = reportsByKey(wrapper)
    expect(byKey.get('pinned:cam')).toEqual({ ...report, spotlit: true })
    expect(byKey.get('them:cam')).toBe(report)
  })
})
