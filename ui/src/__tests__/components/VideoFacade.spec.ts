// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-88 (#1790) P3 — the app's click-to-load video (the tutorial, the SPA
 * landing). The page must load nothing from YouTube until someone presses
 * play: no iframe until the click, then the youtube-nocookie player.
 */
import { describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import VideoFacade from '@/components/common/VideoFacade.vue'
import { embedUrl, FEATURED_VIDEO_ID, videoById } from '@/utils/videos'

const vuetify = createVuetify({ components, directives })
const mountIt = (videoId: string) =>
  mount(VideoFacade, { props: { videoId, poster: '/poster.jpg', caption: 'A tour.' }, global: { plugins: [vuetify] } })

describe('VideoFacade', () => {
  it('shows the poster and loads no player before the click', () => {
    const w = mountIt(FEATURED_VIDEO_ID)
    expect(w.find('iframe').exists()).toBe(false)
    expect(w.find('img').attributes('src')).toBe('/poster.jpg')
    expect(w.find('button').attributes('aria-label')).toContain(videoById(FEATURED_VIDEO_ID)!.title)
    expect(w.text()).toContain('9:03')
    expect(w.text()).toContain('A tour.')
    expect(w.html()).not.toContain('youtube')
  })

  it('swaps in the youtube-nocookie player on the click', async () => {
    const w = mountIt(FEATURED_VIDEO_ID)
    await w.find('button').trigger('click')
    const f = w.find('iframe')
    expect(f.exists()).toBe(true)
    expect(f.attributes('src')).toBe(embedUrl(FEATURED_VIDEO_ID))
    expect(f.attributes('allow')).toContain('autoplay')
    expect(w.find('button').exists()).toBe(false)
  })

  it('renders nothing for an id the manifest does not hold', () => {
    const w = mountIt('dQw4w9WgXcQ')
    expect(w.find('figure').exists()).toBe(false)
  })
})
