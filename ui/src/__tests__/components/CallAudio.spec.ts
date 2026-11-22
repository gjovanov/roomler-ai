// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3563 — every remote participant in a call is heard, whatever the layout
// renders.
//
// A remote participant used to be heard only while their tile's `<video>` was
// rendered: that unmuted element was the only thing playing their audio. So
// nobody past the tiled layout's cap, nobody "Hide participants without video"
// left out, and nobody beyond the mini view's three tiles was heard, although
// the server sends every audio consumer. Now the call owns the sound: one
// hidden `<audio>` per remote stream key that carries audio, for the whole
// call, whatever the layout renders. The tiles show pictures only.
import { describe, it, expect, vi, beforeAll, beforeEach, afterEach } from 'vitest'
import { defineComponent, h } from 'vue'
import { mount, flushPromises, type VueWrapper } from '@vue/test-utils'
import { setActivePinia, createPinia } from 'pinia'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import type { LayoutParticipant } from '@/composables/useConferenceLayout'

vi.mock('@/stores/ws', () => ({
  useWsStore: () => ({ send: vi.fn(), onMediaMessage: vi.fn(), offMediaMessage: vi.fn(), waitForMessage: vi.fn() }),
}))
// jsdom has no canvas to sample; the tiles' black-picture check is not the
// question here.
vi.mock('@/utils/blackPicture', () => ({ watchBlackPicture: () => () => {} }))

import CallAudio from '@/components/conference/CallAudio.vue'
import TiledLayout from '@/components/conference/layouts/TiledLayout.vue'
import { useConferenceStore } from '@/stores/conference'

const vuetify = createVuetify({ components, directives })

// ── Fake media: jsdom has no MediaStream ──────────────────────────────────

interface FakeTrack {
  id: string
  kind: 'audio' | 'video'
  enabled: boolean
  readyState: 'live' | 'ended'
  muted: boolean
}

function track(kind: 'audio' | 'video', id: string): FakeTrack {
  return { id, kind, enabled: true, readyState: 'live', muted: false }
}

class FakeMediaStream {
  private readonly tracks: FakeTrack[]
  onaddtrack: unknown = null
  onremovetrack: unknown = null
  constructor(tracks: FakeTrack[] = []) {
    this.tracks = [...tracks]
  }
  getTracks() {
    return [...this.tracks]
  }
  getAudioTracks() {
    return this.tracks.filter((t) => t.kind === 'audio')
  }
  getVideoTracks() {
    return this.tracks.filter((t) => t.kind === 'video')
  }
}

function stream(tracks: FakeTrack[]): MediaStream {
  return new FakeMediaStream(tracks) as unknown as MediaStream
}

/** A remote stream key as `consumeProducer` stores it. */
function remote(key: string, has: { audio?: boolean; video?: boolean } = { audio: true, video: true }) {
  const tracks: FakeTrack[] = []
  if (has.audio) tracks.push(track('audio', `${key}-mic`))
  if (has.video) tracks.push(track('video', `${key}-cam`))
  return { userId: `u-${key}`, connectionId: key, stream: stream(tracks), kind: 'video', source: 'camera' }
}

// ── What the browser answers to play() ────────────────────────────────────

type Answer = 'ok' | 'NotAllowedError' | 'AbortError' | 'boom'

// Which element is playing, and what it was playing when play() last
// succeeded: a changed srcObject needs its own play().
const playingWhat = new WeakMap<HTMLMediaElement, unknown>()

function playAnswers(rule: (el: HTMLMediaElement) => Answer) {
  return vi.spyOn(HTMLMediaElement.prototype, 'play').mockImplementation(function (this: HTMLMediaElement) {
    const a = rule(this)
    if (a === 'ok') {
      playingWhat.set(this, this.srcObject)
      return Promise.resolve()
    }
    playingWhat.delete(this)
    return Promise.reject(a === 'boom' ? new Error('boom') : new DOMException('refused', a))
  })
}

function isPlaying(el: HTMLMediaElement): boolean {
  return playingWhat.has(el) && playingWhat.get(el) === el.srcObject && el.srcObject != null
}

// ── The call as a page shows it: the audio owner beside a layout ──────────

function participantFor(key: string): LayoutParticipant {
  const r = useConferenceStore().remoteStreams.get(key)!
  return {
    streamKey: key,
    userId: r.userId,
    displayName: key,
    stream: r.stream,
    isMuted: false,
    isLocal: false,
    isScreenShare: false,
    isPinned: false,
    videoPaused: false,
    audioLevel: 0,
  }
}

/** The owner, and a tiled layout that renders only `rendered` (past its cap,
 *  or the rest hidden by "Hide participants without video"). */
function mountCall(rendered: string[]) {
  const Call = defineComponent({
    setup: () => () =>
      h('div', [
        h(CallAudio),
        h(TiledLayout, {
          participants: rendered.filter((k) => useConferenceStore().remoteStreams.has(k)).map(participantFor),
          selfParticipant: null,
          selfViewFloating: false,
          selfViewMode: 'in-grid-cropped',
          activeSpeakerKey: null,
        }),
      ]),
  })
  return mount(Call, { global: { plugins: [vuetify] } })
}

function mediaElements(w: VueWrapper): HTMLMediaElement[] {
  return [...w.element.querySelectorAll<HTMLMediaElement>('audio, video')]
}

/** Every element on the page that plays this track out loud. */
function audibleOf(w: VueWrapper, t: { id: string }): HTMLMediaElement[] {
  return mediaElements(w).filter((el) => {
    const src = el.srcObject as MediaStream | null
    return !el.muted && isPlaying(el) && !!src?.getAudioTracks().some((a) => a.id === t.id)
  })
}

function sinks(w: VueWrapper): HTMLAudioElement[] {
  return [...w.element.querySelectorAll<HTMLAudioElement>('audio[data-stream-key]')]
}

function sinkKeys(w: VueWrapper): string[] {
  return sinks(w)
    .map((el) => el.dataset.streamKey ?? '')
    .sort()
}

function sinkOf(w: VueWrapper, key: string): HTMLAudioElement {
  const el = sinks(w).find((s) => s.dataset.streamKey === key)
  if (!el) throw new Error(`no sink for ${key}`)
  return el
}

function click() {
  document.body.dispatchEvent(new MouseEvent('click', { bubbles: true }))
}

beforeAll(() => {
  vi.stubGlobal('MediaStream', FakeMediaStream)
  globalThis.ResizeObserver ??= class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver
})

beforeEach(() => {
  setActivePinia(createPinia())
})

afterEach(() => {
  vi.restoreAllMocks()
})

describe('CallAudio — every remote participant is heard, whatever the layout renders (#3563)', () => {
  it('five remote streams with audio, a layout that renders two: five sinks play, each stream once, and no rendered remote tile plays sound', async () => {
    playAnswers(() => 'ok')
    const store = useConferenceStore()
    const keys = ['c1', 'c2', 'c3', 'c4', 'c5']
    for (const k of keys) store.remoteStreams.set(k, remote(k))

    const w = mountCall(['c1', 'c2'])
    await flushPromises()

    expect(sinkKeys(w)).toEqual(keys)
    for (const k of keys) {
      const mic = store.remoteStreams.get(k)!.stream.getAudioTracks()[0]
      const players = audibleOf(w, mic)
      expect(players, `${k} is heard exactly once`).toHaveLength(1)
      expect(players[0].tagName).toBe('AUDIO')
      expect(players[0].dataset.userId).toBe(`u-${k}`)
    }
    // The two rendered tiles show pictures only.
    const tiles = [...w.element.querySelectorAll<HTMLVideoElement>('video[data-stream-key]')]
    expect(tiles.map((t) => t.dataset.streamKey).sort()).toEqual(['c1', 'c2'])
    for (const t of tiles) expect(t.muted, `the ${t.dataset.streamKey} tile is muted`).toBe(true)
    w.unmount()
  })

  it('a stream key without audio gets no sink; a shared screen with audio gets its own', async () => {
    playAnswers(() => 'ok')
    const store = useConferenceStore()
    store.remoteStreams.set('c1', remote('c1', { video: true }))
    store.remoteStreams.set('c2:screen', { ...remote('c2:screen'), source: 'screen' })
    const w = mountCall([])
    await flushPromises()
    expect(sinkKeys(w)).toEqual(['c2:screen'])
    w.unmount()
  })

  it('the sinks follow the streams: a late audio track adds one, a leaver removes one, the end of the call stops them all', async () => {
    const play = playAnswers(() => 'ok')
    const store = useConferenceStore()
    store.remoteStreams.set('c1', remote('c1'))
    // c2's tile mounted on its camera; its audio consumer has not arrived yet.
    store.remoteStreams.set('c2', remote('c2', { video: true }))
    const w = mountCall(['c1', 'c2'])
    await flushPromises()
    expect(sinkKeys(w)).toEqual(['c1'])
    const c1Sink = sinkOf(w, 'c1')
    const c1Src = c1Sink.srcObject

    // The audio consumer arrives: like `consumeProducer`, the store REPLACES
    // the entry with a new stream of the old tracks plus the new one.
    const late = track('audio', 'c2-mic')
    const c2 = store.remoteStreams.get('c2')!
    store.remoteStreams.set('c2', { ...c2, stream: stream([...(c2.stream.getTracks() as unknown as FakeTrack[]), late]) })
    await flushPromises()
    expect(sinkKeys(w)).toEqual(['c1', 'c2'])
    expect(audibleOf(w, late)).toHaveLength(1)

    // A video track that joins c1's key replaces its stream too, but its sound
    // is not restarted: same audio track, same sink stream, no new play() on
    // the sink (the tile restarts its picture, as it always has).
    const sinkPlays = () => play.mock.contexts.filter((el) => el === c1Sink).length
    const sinkPlaysBefore = sinkPlays()
    const c1 = store.remoteStreams.get('c1')!
    store.remoteStreams.set('c1', {
      ...c1,
      stream: stream([...(c1.stream.getTracks() as unknown as FakeTrack[]), track('video', 'c1-cam2')]),
    })
    await flushPromises()
    expect(sinkOf(w, 'c1')).toBe(c1Sink)
    expect(c1Sink.srcObject).toBe(c1Src)
    expect(sinkPlays()).toBe(sinkPlaysBefore)

    // c1 leaves (`handlePeerLeft` deletes its keys).
    store.remoteStreams.delete('c1')
    await flushPromises()
    expect(sinkKeys(w)).toEqual(['c2'])
    expect(c1Sink.srcObject).toBeNull()

    // The call ends.
    const c2Sink = sinkOf(w, 'c2')
    store.leaveRoom()
    await flushPromises()
    expect(sinkKeys(w)).toEqual([])
    expect(c2Sink.srcObject).toBeNull()
    w.unmount()
  })

  it('a refused audible start shows "Tap for sound" once, and the next click restores every sink; the local stream never gets a sink', async () => {
    const play = playAnswers((el) => (el.muted ? 'ok' : 'NotAllowedError'))
    const store = useConferenceStore()
    const localMic = track('audio', 'me-mic')
    store.localStream = stream([localMic, track('video', 'me-cam')])
    for (const k of ['c1', 'c2', 'c3']) store.remoteStreams.set(k, remote(k))
    const w = mountCall(['c1'])
    await flushPromises()

    // Said once, for the whole call — not once per participant.
    expect(w.findAll('[data-testid="tap-for-sound"]')).toHaveLength(1)
    // Every sink plays muted meanwhile (the pipeline, and the speaker
    // detection that reads it, keep running).
    expect(sinkKeys(w)).toEqual(['c1', 'c2', 'c3'])
    for (const s of sinks(w)) {
      expect(s.muted).toBe(true)
      expect(isPlaying(s)).toBe(true)
    }
    // Your own microphone is never played back.
    expect(sinks(w).some((s) => (s.srcObject as MediaStream).getAudioTracks().some((a) => a.id === localMic.id))).toBe(false)
    expect(audibleOf(w, localMic)).toHaveLength(0)

    // A participant who joins before the gesture starts muted too.
    store.remoteStreams.set('c4', remote('c4'))
    await flushPromises()
    expect(sinkOf(w, 'c4').muted).toBe(true)
    expect(w.findAll('[data-testid="tap-for-sound"]')).toHaveLength(1)

    // The page has its gesture now.
    play.mockImplementation(function (this: HTMLMediaElement) {
      playingWhat.set(this, this.srcObject)
      return Promise.resolve()
    })
    click()
    await flushPromises()
    for (const k of ['c1', 'c2', 'c3', 'c4']) {
      const mic = store.remoteStreams.get(k)!.stream.getAudioTracks()[0]
      expect(audibleOf(w, mic), `${k} is heard after the click`).toHaveLength(1)
    }
    expect(w.find('[data-testid="tap-for-sound"]').exists()).toBe(false)
    expect(audibleOf(w, localMic)).toHaveLength(0)
    w.unmount()
  })

  it('a sink refused after the first gesture (a late joiner, a new element) is muted alone, says so again, and the next click brings it back', async () => {
    // Some browsers judge each new element: a joiner's sink can be refused
    // although the others already play.
    const play = playAnswers((el) => (el.muted || el.dataset.streamKey !== 'c2' ? 'ok' : 'NotAllowedError'))
    const store = useConferenceStore()
    store.remoteStreams.set('c1', remote('c1'))
    const w = mountCall([])
    await flushPromises()
    expect(w.find('[data-testid="tap-for-sound"]').exists()).toBe(false)

    store.remoteStreams.set('c2', remote('c2'))
    await flushPromises()
    expect(sinkOf(w, 'c1').muted).toBe(false)
    expect(sinkOf(w, 'c2').muted).toBe(true)
    expect(isPlaying(sinkOf(w, 'c2'))).toBe(true)
    expect(w.findAll('[data-testid="tap-for-sound"]')).toHaveLength(1)

    play.mockImplementation(function (this: HTMLMediaElement) {
      playingWhat.set(this, this.srcObject)
      return Promise.resolve()
    })
    document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', bubbles: true }))
    await flushPromises()
    expect(sinkOf(w, 'c2').muted).toBe(false)
    expect(isPlaying(sinkOf(w, 'c2'))).toBe(true)
    expect(w.find('[data-testid="tap-for-sound"]').exists()).toBe(false)
    w.unmount()
  })

  it("the call's own button brings the sound back too", async () => {
    const play = playAnswers((el) => (el.muted ? 'ok' : 'NotAllowedError'))
    const store = useConferenceStore()
    store.remoteStreams.set('c1', remote('c1'))
    const w = mountCall([])
    await flushPromises()
    play.mockImplementation(function (this: HTMLMediaElement) {
      playingWhat.set(this, this.srcObject)
      return Promise.resolve()
    })
    await w.find('[data-testid="tap-for-sound"]').trigger('click')
    await flushPromises()
    expect(sinkOf(w, 'c1').muted).toBe(false)
    expect(w.find('[data-testid="tap-for-sound"]').exists()).toBe(false)
    w.unmount()
  })

  it('AbortError (a newer srcObject took over) is not a refusal; any other failure is logged', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    playAnswers((el) => (el.dataset.streamKey === 'c1' ? 'AbortError' : 'boom'))
    const store = useConferenceStore()
    store.remoteStreams.set('c1', remote('c1'))
    store.remoteStreams.set('c2', remote('c2'))
    const w = mountCall([])
    await flushPromises()
    expect(sinkOf(w, 'c1').muted).toBe(false)
    expect(w.find('[data-testid="tap-for-sound"]').exists()).toBe(false)
    expect(warn).toHaveBeenCalledTimes(1)
    expect(warn).toHaveBeenCalledWith('[CallAudio] play() failed:', expect.any(Error))
    w.unmount()
  })

  it('unmounted, it drops every sink and stops listening for the gesture', async () => {
    const play = playAnswers((el) => (el.muted ? 'ok' : 'NotAllowedError'))
    const store = useConferenceStore()
    store.remoteStreams.set('c1', remote('c1'))
    const w = mountCall([])
    await flushPromises()
    const sink = sinkOf(w, 'c1')
    w.unmount()
    expect(sink.srcObject).toBeNull()
    const before = play.mock.calls.length
    click()
    await flushPromises()
    expect(play.mock.calls.length).toBe(before)
  })
})
