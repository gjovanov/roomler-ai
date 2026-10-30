// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-124 — a black tile says why.
//
// 1. A picture that plays with nothing in it (a covered lens) is named on its
//    tile, in text, and the tile reports it so the call page can warn. A camera
//    turned OFF is not a covered camera: a disabled track, a peer's FR-30
//    camera-off signal and a shared screen are never judged.
// 2. A tile the browser refused to start (autoplay policy) used to stay black:
//    `play().catch(() => {})` swallowed the NotAllowedError. Now the picture
//    comes back muted at once, the tile offers its sound, and the next gesture
//    anywhere brings the sound back.
import { describe, it, expect, vi, afterEach } from 'vitest'
import { defineComponent, h, ref } from 'vue'
import { mount, flushPromises } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import type { WatchBlackPictureOptions } from '@/utils/blackPicture'

// The detector is unit-tested on its own (utils/blackPicture.spec.ts); here the
// question is what the TILE does with its verdict, so the verdict is driven by
// hand. jsdom has no canvas to sample anyway.
const { watchers } = vi.hoisted(() => ({
  watchers: [] as { opts: WatchBlackPictureOptions; black: boolean; stopped: boolean }[],
}))
vi.mock('@/utils/blackPicture', () => ({
  watchBlackPicture: (_video: HTMLVideoElement, opts: WatchBlackPictureOptions) => {
    const w = { opts, black: false, stopped: false }
    watchers.push(w)
    return () => {
      w.stopped = true
      if (w.black) {
        w.black = false
        opts.onChange(false)
      }
    }
  },
}))

import VideoTile from '@/components/conference/VideoTile.vue'

const vuetify = createVuetify({ components, directives })

interface FakeTrack {
  kind: string
  enabled: boolean
  readyState: string
  muted: boolean
}

function fakeStream(track: Partial<FakeTrack> = {}) {
  const t = { kind: 'video', enabled: true, readyState: 'live', muted: false, ...track }
  const stream = {
    getVideoTracks: () => [t],
    getAudioTracks: () => [],
    getTracks: () => [t],
    onaddtrack: null,
    onremovetrack: null,
  } as unknown as MediaStream
  return { stream, track: t }
}

function mountTile(props: Record<string, unknown> = {}) {
  return mount(VideoTile, {
    props: {
      stream: fakeStream().stream,
      displayName: 'Them',
      isMuted: false,
      isLocal: false,
      streamKey: 'them:cam',
      ...props,
    },
    global: { plugins: [vuetify] },
  })
}

/** What the browser answers to `play()`, decided per call from the element. */
type Answer = 'ok' | 'NotAllowedError' | 'AbortError' | 'boom'
function playAnswers(rule: (el: HTMLMediaElement) => Answer) {
  return vi
    .spyOn(HTMLMediaElement.prototype, 'play')
    .mockImplementation(function (this: HTMLMediaElement) {
      const a = rule(this)
      if (a === 'ok') return Promise.resolve()
      return Promise.reject(a === 'boom' ? new Error('boom') : new DOMException('refused', a))
    })
}

function lastWatcher() {
  return watchers[watchers.length - 1]
}

function setBlack(black: boolean) {
  const w = lastWatcher()
  w.black = black
  w.opts.onChange(black)
}

afterEach(() => {
  vi.restoreAllMocks()
  watchers.length = 0
})

describe('VideoTile — a black picture is named (FR-124)', () => {
  it('your own tile says your camera is sending a black picture, and asks about the lens', async () => {
    playAnswers(() => 'ok')
    const w = mountTile({ isLocal: true, displayName: 'Me', streamKey: 'local' })
    await flushPromises()
    expect(w.find('[data-testid="black-picture-notice"]').exists()).toBe(false)

    setBlack(true)
    await flushPromises()
    const notice = w.find('[data-testid="black-picture-notice"]')
    expect(notice.text()).toContain('Your camera is sending a black picture')
    expect(notice.text()).toContain('Is the lens covered?')
    expect(w.emitted('picture-black')).toEqual([[true]])

    setBlack(false)
    await flushPromises()
    expect(w.find('[data-testid="black-picture-notice"]').exists()).toBe(false)
    expect(w.emitted('picture-black')).toEqual([[true], [false]])
  })

  it("a peer's tile says the black comes from that camera", async () => {
    playAnswers(() => 'ok')
    const w = mountTile()
    await flushPromises()
    setBlack(true)
    await flushPromises()
    const text = w.find('[data-testid="black-picture-notice"]').text()
    expect(text).toContain('Black picture from this camera')
    expect(text).toContain('Its lens may be covered')
  })

  it('a compact tile says it in two words', async () => {
    playAnswers(() => 'ok')
    const w = mountTile({ compact: true, isLocal: true, streamKey: 'local' })
    await flushPromises()
    setBlack(true)
    await flushPromises()
    const text = w.find('[data-testid="black-picture-notice"]').text()
    expect(text).toContain('Black picture')
    expect(text).not.toContain('Your camera')
    expect(text).not.toContain('lens')
  })

  it('judges a playing, enabled, live camera track', async () => {
    playAnswers(() => 'ok')
    mountTile()
    await flushPromises()
    expect(lastWatcher().opts.isActive()).toBe(true)
  })

  it.each<[string, Record<string, unknown>]>([
    ['your camera turned off (a disabled track)', { stream: fakeStream({ enabled: false }).stream }],
    ['a track receiving nothing (muted)', { stream: fakeStream({ muted: true }).stream }],
    ['an ended track', { stream: fakeStream({ readyState: 'ended' }).stream }],
    ["a peer who said their camera is off (FR-30)", { videoPaused: true }],
    ['a shared screen (a black slide is not a covered lens)', { isScreenShare: true }],
    ['no stream at all', { stream: null }],
  ])('never judges %s', async (_name, props) => {
    playAnswers(() => 'ok')
    // A real element keeps painting at full size after its track is disabled
    // or ends (black, or the last frame), so the tile's own `hasVideoTrack`
    // stays true and only the camera-state checks stand between a camera
    // turned OFF and a "covered lens" notice. Model that, or this passes on
    // jsdom's 0 px for the wrong reason.
    vi.spyOn(HTMLVideoElement.prototype, 'videoWidth', 'get').mockReturnValue(640)
    mountTile(props)
    await flushPromises()
    expect(lastWatcher().opts.isActive()).toBe(false)
  })

  it("a peer's camera-off signal wins over a black verdict: the avatar, not the notice", async () => {
    playAnswers(() => 'ok')
    const w = mountTile()
    await flushPromises()
    setBlack(true)
    // ⚠️ The cast is not decoration. The hosted image runs vue-tsc in oven/bun,
    // which has no node, so `.vue` resolves through env.d.ts's shim and its
    // props are `object`: a literal naming a REAL prop then fails the
    // excess-property check there and nowhere else. Without it this broke
    // every hosted-image build on master (69b598136, 2026-10-26). Same trap
    // as KeepBusyMenu.spec.ts.
    await w.setProps({ videoPaused: true } as unknown as Parameters<typeof w.setProps>[0])
    expect(w.find('[data-testid="black-picture-notice"]').exists()).toBe(false)
  })

  it('a black tile that unmounts tells its parent it is gone', async () => {
    // Through a real parent, the way the layouts listen: the report must reach
    // a listener while the tile is going away, or the call page keeps warning
    // about a tile that no longer exists.
    playAnswers(() => 'ok')
    const heard: boolean[] = []
    const show = ref(true)
    const stream = fakeStream().stream
    const Parent = defineComponent({
      setup: () => () =>
        show.value
          ? h(VideoTile, {
              stream,
              displayName: 'Me',
              isMuted: false,
              isLocal: true,
              streamKey: 'local',
              onPictureBlack: (b: boolean) => heard.push(b),
            })
          : null,
    })
    const w = mount(Parent, { global: { plugins: [vuetify] } })
    await flushPromises()
    setBlack(true)
    show.value = false
    await flushPromises()
    expect(lastWatcher().stopped).toBe(true)
    expect(heard).toEqual([true, false])
    w.unmount()
  })
})

describe('VideoTile — a refused start recovers (FR-124)', () => {
  it('refused with sound: plays muted at once, offers the sound, and the next click anywhere restores it', async () => {
    const play = playAnswers((el) => (el.muted ? 'ok' : 'NotAllowedError'))
    const w = mountTile()
    await flushPromises()

    const video = w.find('video').element as HTMLVideoElement
    expect(video.muted).toBe(true)
    expect(w.find('[data-testid="tap-for-sound"]').exists()).toBe(true)
    expect(w.find('[data-testid="tap-to-play"]').exists()).toBe(false)

    // The page has its gesture now: the browser lets it play with sound.
    play.mockImplementation(() => Promise.resolve())
    document.body.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await flushPromises()

    expect(video.muted).toBe(false)
    expect(w.find('[data-testid="tap-for-sound"]').exists()).toBe(false)
    w.unmount()
  })

  it("the tile's own button brings the sound back too", async () => {
    const play = playAnswers((el) => (el.muted ? 'ok' : 'NotAllowedError'))
    const w = mountTile()
    await flushPromises()
    play.mockImplementation(() => Promise.resolve())
    await w.find('[data-testid="tap-for-sound"]').trigger('click')
    await flushPromises()
    expect((w.find('video').element as HTMLVideoElement).muted).toBe(false)
    expect(w.find('[data-testid="tap-for-sound"]').exists()).toBe(false)
    w.unmount()
  })

  it('refused even muted: "Tap to play", and a tap starts it', async () => {
    const play = playAnswers(() => 'NotAllowedError')
    const w = mountTile()
    await flushPromises()
    expect(w.find('[data-testid="tap-to-play"]').exists()).toBe(true)
    expect(w.find('[data-testid="tap-for-sound"]').exists()).toBe(false)

    play.mockImplementation(() => Promise.resolve())
    await w.find('[data-testid="tap-to-play"]').trigger('click')
    await flushPromises()
    expect(w.find('[data-testid="tap-to-play"]').exists()).toBe(false)
    expect((w.find('video').element as HTMLVideoElement).muted).toBe(false)
    w.unmount()
  })

  it('your own (muted) tile refused: "Tap to play", and it stays muted after', async () => {
    const play = playAnswers(() => 'NotAllowedError')
    const w = mountTile({ isLocal: true, streamKey: 'local' })
    await flushPromises()
    expect(w.find('[data-testid="tap-to-play"]').exists()).toBe(true)
    play.mockImplementation(() => Promise.resolve())
    document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', bubbles: true }))
    await flushPromises()
    expect(w.find('[data-testid="tap-to-play"]').exists()).toBe(false)
    expect((w.find('video').element as HTMLVideoElement).muted).toBe(true)
    w.unmount()
  })

  it('AbortError (a newer srcObject took over) is not a refusal', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    playAnswers(() => 'AbortError')
    const w = mountTile()
    await flushPromises()
    expect((w.find('video').element as HTMLVideoElement).muted).toBe(false)
    expect(w.find('[data-testid="tap-for-sound"]').exists()).toBe(false)
    expect(w.find('[data-testid="tap-to-play"]').exists()).toBe(false)
    expect(warn).not.toHaveBeenCalled()
    w.unmount()
  })

  it('any other failure is logged, not hidden', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    playAnswers(() => 'boom')
    const w = mountTile()
    await flushPromises()
    expect(warn).toHaveBeenCalledWith('[VideoTile] play() failed:', expect.any(Error))
    expect(w.find('[data-testid="tap-to-play"]').exists()).toBe(false)
    w.unmount()
  })

  it('an unmounted tile stops listening for the gesture', async () => {
    const play = playAnswers((el) => (el.muted ? 'ok' : 'NotAllowedError'))
    const w = mountTile()
    await flushPromises()
    w.unmount()
    const before = play.mock.calls.length
    document.body.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await flushPromises()
    expect(play.mock.calls.length).toBe(before)
  })
})
