// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-116 P4b — a tile's actions (pin, picture-in-picture, fullscreen) on a
// touch screen.
//
// They lived in an overlay revealed by `:hover` and nothing else. A phone has
// no hover, so they were unreachable — while the overlay itself still covered
// the whole tile at opacity 0, so a tap in the middle of a face hit an
// invisible picture-in-picture button. Now a tap shows them on a `(hover:
// none)` screen, a second tap hides them, and pressing an action never toggles
// the overlay underneath it.
import { describe, it, expect, afterEach, vi } from 'vitest'
import { mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

import VideoTile from '@/components/conference/VideoTile.vue'

const vuetify = createVuetify({ components, directives })
const realMatchMedia = window.matchMedia

/** The tile reads `(hover: none)` once, at setup — stub it before mounting. */
function screen(kind: 'touch' | 'mouse') {
  window.matchMedia = vi.fn((query: string) => ({
    matches: kind === 'touch' && query === '(hover: none)',
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia
}

function mountTile(props: Record<string, unknown> = {}) {
  return mount(VideoTile, {
    props: {
      stream: null,
      displayName: 'Them',
      isMuted: false,
      isLocal: false,
      streamKey: 'them:cam',
      showActions: true,
      ...props,
    },
    global: { plugins: [vuetify] },
  })
}

afterEach(() => {
  window.matchMedia = realMatchMedia
  delete (document as { pictureInPictureEnabled?: boolean }).pictureInPictureEnabled
})

describe('VideoTile — actions on a touch screen (FR-116 P4b)', () => {
  it('a tap shows the actions and a second tap hides them', async () => {
    screen('touch')
    const w = mountTile()
    const overlay = () => w.find('.action-overlay')
    expect(overlay().classes()).not.toContain('action-overlay--open')

    await w.find('.video-container').trigger('click')
    expect(overlay().classes()).toContain('action-overlay--open')

    await w.find('.video-container').trigger('click')
    expect(overlay().classes()).not.toContain('action-overlay--open')
  })

  it('pressing an action does its job and leaves the overlay open', async () => {
    screen('touch')
    const w = mountTile()
    await w.find('.video-container').trigger('click')

    await w.find('.action-overlay button').trigger('click')
    expect(w.emitted('toggle-pin')).toEqual([['them:cam']])
    expect(w.find('.action-overlay').classes()).toContain('action-overlay--open')
  })

  it('with a mouse a click arms nothing — hover reveals the actions', async () => {
    screen('mouse')
    const w = mountTile()
    await w.find('.video-container').trigger('click')
    expect(w.find('.action-overlay').classes()).not.toContain('action-overlay--open')
  })

  it('a tile without actions has no overlay to open', async () => {
    screen('touch')
    const w = mountTile({ showActions: false })
    await w.find('.video-container').trigger('click')
    expect(w.find('.action-overlay').exists()).toBe(false)
  })
})

describe('VideoTile — picture-in-picture only where the browser has it', () => {
  it('hides the button where the browser has no picture-in-picture', () => {
    screen('mouse')
    const w = mountTile()
    expect(w.find('[title="Picture-in-picture"]').exists()).toBe(false)
  })

  it('shows it where the browser does', () => {
    screen('mouse')
    Object.defineProperty(document, 'pictureInPictureEnabled', { value: true, configurable: true })
    const w = mountTile()
    expect(w.find('[title="Picture-in-picture"]').exists()).toBe(true)
  })
})
