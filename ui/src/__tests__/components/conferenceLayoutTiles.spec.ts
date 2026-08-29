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
