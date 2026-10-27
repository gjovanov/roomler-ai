// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-127 P1 — a remote tile says whether it is on screen, so the server can
// pause the video of the tiles that are not: an IntersectionObserver on the
// tile's root, one report per change, the size in device pixels, `false` on
// unmount while the parent still listens, and nothing at all from a local
// tile or in a browser without the observer.
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { defineComponent, h, ref } from 'vue'
import { mount, flushPromises } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

import VideoTile from '@/components/conference/VideoTile.vue'
import type { TileVisibility } from '@/utils/viewReport'

const vuetify = createVuetify({ components, directives })

type Callback = (entries: IntersectionObserverEntry[], observer: IntersectionObserver) => void

/** The observer, driven by hand. */
class FakeObserver {
  static all: FakeObserver[] = []
  targets: Element[] = []
  disconnected = false
  constructor(private cb: Callback) {
    FakeObserver.all.push(this)
  }
  observe(el: Element) {
    this.targets.push(el)
  }
  unobserve() {}
  disconnect() {
    this.disconnected = true
  }
  takeRecords() {
    return []
  }
  fire(isIntersecting: boolean, width = 160, height = 90) {
    this.cb(
      [{ isIntersecting, boundingClientRect: { width, height } } as unknown as IntersectionObserverEntry],
      this as unknown as IntersectionObserver,
    )
  }
}

function mountTile(props: Record<string, unknown> = {}) {
  return mount(VideoTile, {
    props: {
      stream: null,
      displayName: 'Them',
      isMuted: false,
      isLocal: false,
      streamKey: 'them:cam',
      ...props,
    },
    global: { plugins: [vuetify] },
  })
}

const reports = (w: ReturnType<typeof mountTile>) =>
  (w.emitted('visible') ?? []).map(([r]) => r as TileVisibility)

beforeEach(() => {
  FakeObserver.all.length = 0
  vi.stubGlobal('IntersectionObserver', FakeObserver)
  vi.stubGlobal('devicePixelRatio', 2)
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('VideoTile — on screen or not (FR-127 P1)', () => {
  it('a remote tile observes its own root and reports coming on screen, in device pixels', async () => {
    const w = mountTile()
    await flushPromises()
    expect(FakeObserver.all).toHaveLength(1)
    const io = FakeObserver.all[0]
    expect(io.targets).toEqual([w.element])

    io.fire(true, 160, 90)
    expect(reports(w)).toEqual([{ visible: true, w: 320, h: 180 }])
    w.unmount()
  })

  it('reports a change once, not every observation', async () => {
    const w = mountTile()
    await flushPromises()
    const io = FakeObserver.all[0]
    io.fire(true)
    io.fire(true)
    io.fire(false, 0, 0)
    io.fire(false, 0, 0)
    io.fire(true)
    expect(reports(w).map((r) => r.visible)).toEqual([true, false, true])
    w.unmount()
  })

  it('a tile that was on screen says so when it unmounts, while its parent listens, and stops observing', async () => {
    const heard: TileVisibility[] = []
    const show = ref(true)
    const Parent = defineComponent({
      setup: () => () =>
        show.value
          ? h(VideoTile, {
              stream: null,
              displayName: 'Them',
              isMuted: false,
              isLocal: false,
              streamKey: 'them:cam',
              onVisible: (r: TileVisibility) => heard.push(r),
            })
          : null,
    })
    const w = mount(Parent, { global: { plugins: [vuetify] } })
    await flushPromises()
    const io = FakeObserver.all[0]
    io.fire(true, 160, 90)
    show.value = false
    await flushPromises()
    expect(io.disconnected).toBe(true)
    expect(heard).toEqual([
      { visible: true, w: 320, h: 180 },
      { visible: false, w: 0, h: 0 },
    ])
    w.unmount()
  })

  it('a tile that was off screen says nothing more when it unmounts', async () => {
    const w = mountTile()
    await flushPromises()
    FakeObserver.all[0].fire(false, 0, 0)
    expect(reports(w)).toEqual([])
    w.unmount()
    expect(reports(w)).toEqual([])
  })

  it('a tile in picture-in-picture stays reported on screen while its strip scrolls it out (review)', async () => {
    const w = mountTile()
    await flushPromises()
    const io = FakeObserver.all[0]
    const video = w.find('video').element as HTMLVideoElement
    io.fire(true, 160, 90)
    expect(reports(w).map((r) => r.visible)).toEqual([true])

    // The floating window opens on this tile, then the strip scrolls it out:
    // the picture is still on the user's screen.
    Object.defineProperty(document, 'pictureInPictureElement', { configurable: true, get: () => video })
    video.dispatchEvent(new Event('enterpictureinpicture'))
    io.fire(false, 0, 0)
    expect(reports(w).map((r) => r.visible)).toEqual([true])

    // The window closes while the tile is still scrolled out: now it is off.
    Object.defineProperty(document, 'pictureInPictureElement', { configurable: true, get: () => null })
    video.dispatchEvent(new Event('leavepictureinpicture'))
    expect(reports(w).map((r) => r.visible)).toEqual([true, false])
    w.unmount()
  })

  it('a local tile never observes: nothing is received for it', async () => {
    const w = mountTile({ isLocal: true, streamKey: 'local', displayName: 'Me' })
    await flushPromises()
    expect(FakeObserver.all).toHaveLength(0)
    w.unmount()
    expect(reports(w)).toEqual([])
  })

  it('a browser without IntersectionObserver gets a tile that mounts and reports nothing', async () => {
    vi.stubGlobal('IntersectionObserver', undefined)
    const w = mountTile()
    await flushPromises()
    expect(w.find('video').exists()).toBe(true)
    w.unmount()
    expect(reports(w)).toEqual([])
  })
})
