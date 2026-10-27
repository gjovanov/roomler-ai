// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-127 P2 — simulcast, the store's half: the camera publishes the ladder
// the server announced (and tells it the capture size), one encoding when
// no ladder was announced or the browser refused it, the tiles' reports go
// out when only the ladder is on, a spotlit tile is marked in the report,
// and a resized tile moves its size without being counted twice.
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { flushPromises } from '@vue/test-utils'

// The socket, as the store sees it: every `send` is recorded, the handlers
// the store registers are kept so a test can deliver `media:new_producer`,
// and `waitForMessage` answers at once with the canned frame for its type.
const wire = vi.hoisted(() => ({
  sent: [] as { type: string; data: Record<string, unknown> }[],
  handlers: new Map<string, (data: Record<string, unknown>) => void>(),
  answers: new Map<string, Record<string, unknown>>(),
}))
vi.mock('@/stores/ws', () => ({
  useWsStore: () => ({
    status: 'connected',
    send: (type: string, data: Record<string, unknown>) => {
      wire.sent.push({ type, data })
    },
    onMediaMessage: (type: string, handler: (data: Record<string, unknown>) => void) => {
      wire.handlers.set(type, handler)
    },
    offMediaMessage: (type: string) => {
      wire.handlers.delete(type)
    },
    waitForMessage: (type: string) =>
      wire.answers.has(type)
        ? Promise.resolve(wire.answers.get(type))
        : Promise.reject(new Error(`no canned ${type}`)),
  }),
}))

// mediasoup-client, as far as the store drives it: a send transport that
// records what it was asked to produce and raises the `produce` event the
// real one raises (so the store's `media:produce` goes out), refusing the
// encodings when told to; a recv transport handing back consumers.
const media = vi.hoisted(() => ({
  produced: [] as Record<string, unknown>[],
  refuseEncodings: false,
}))
vi.mock('mediasoup-client', () => {
  const transport = (id: string) => {
    const handlers = new Map<string, (...args: unknown[]) => void>()
    return {
      id,
      on: (event: string, handler: (...args: unknown[]) => void) => {
        handlers.set(event, handler)
      },
      close: () => {},
      produce: async (opts: Record<string, unknown>) => {
        media.produced.push(opts)
        const kind = (opts.track as { kind: string }).kind
        if (media.refuseEncodings && opts.encodings) {
          throw new Error('OperationError: simulcast not supported')
        }
        let producerId = `prod-${kind}`
        const onProduce = handlers.get('produce')
        if (onProduce) {
          await new Promise<void>((resolve, reject) =>
            onProduce(
              { kind, rtpParameters: {}, appData: opts.appData },
              (result: { id: string }) => {
                producerId = result.id
                resolve()
              },
              reject,
            ),
          )
        }
        return { id: producerId, kind, pause: () => {}, resume: () => {}, close: () => {} }
      },
      consume: async (opts: { id: string; producerId: string; kind: string }) => ({
        id: opts.id,
        producerId: opts.producerId,
        kind: opts.kind,
        track: { kind: opts.kind },
        close: () => {},
      }),
    }
  }
  return {
    Device: class {
      rtpCapabilities = {}
      async load() {}
      createSendTransport() {
        return transport('send')
      }
      createRecvTransport() {
        return transport('recv')
      }
    },
  }
})

import { useConferenceStore } from '@/stores/conference'

const ladder = {
  encodings: [
    { scale_resolution_down_by: 2, max_bitrate: 300000, scalability_mode: 'L1T3' },
    { scale_resolution_down_by: 1, max_bitrate: 1500000, scalability_mode: 'L1T3' },
  ],
}
const expectedEncodings = [
  { scaleResolutionDownBy: 2, maxBitrate: 300000, scalabilityMode: 'L1T3' },
  { scaleResolutionDownBy: 1, maxBitrate: 1500000, scalabilityMode: 'L1T3' },
]

const transportCreated = (meeting?: Record<string, unknown>) => ({
  send_transport: { id: 'send', ice_parameters: {}, ice_candidates: [], dtls_parameters: {} },
  recv_transport: { id: 'recv', ice_parameters: {}, ice_candidates: [], dtls_parameters: {} },
  ice_servers: [],
  force_relay: false,
  ...(meeting ? { meeting } : {}),
})

const views = () => wire.sent.filter((m) => m.type === 'media:view')
const produces = () => wire.sent.filter((m) => m.type === 'media:produce')

async function join(meeting?: Record<string, unknown>) {
  wire.answers.set('media:router_capabilities', { rtp_capabilities: {} })
  wire.answers.set('media:transport_created', transportCreated(meeting))
  wire.answers.set('media:produce_result', { id: 'p-1' })
  const store = useConferenceStore()
  await store.joinRoom('t1', 'r1', 'Room')
  return store
}

/** A camera and a microphone, the camera opened at `w`×`h`. */
function camera(w = 640, h = 480) {
  const tracks = [
    { kind: 'audio', stop: () => {} },
    { kind: 'video', stop: () => {}, getSettings: () => ({ width: w, height: h }) },
  ]
  vi.stubGlobal('navigator', {
    mediaDevices: {
      getUserMedia: async () => ({
        getAudioTracks: () => tracks.filter((t) => t.kind === 'audio'),
        getVideoTracks: () => tracks.filter((t) => t.kind === 'video'),
        getTracks: () => tracks,
      }),
    },
  })
}

const videoProduce = () => media.produced.filter((p) => (p.track as { kind: string }).kind === 'video')

/** Deliver a peer's video producer and let the consume chain finish. */
async function peerVideo(connectionId: string, producerId: string) {
  wire.answers.set('media:consumer_created', {
    id: `cons-${producerId}`,
    producer_id: producerId,
    kind: 'video',
    rtp_parameters: {},
    producer_paused: false,
  })
  wire.handlers.get('media:new_producer')!({
    producer_id: producerId,
    user_id: 'u-' + connectionId,
    connection_id: connectionId,
    kind: 'video',
    source: 'camera',
  })
  await flushPromises()
}

class FakeIntersectionObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
  takeRecords() {
    return []
  }
}

beforeEach(() => {
  setActivePinia(createPinia())
  vi.useFakeTimers()
  wire.sent.length = 0
  wire.handlers.clear()
  wire.answers.clear()
  media.produced.length = 0
  media.refuseEncodings = false
  vi.stubGlobal('IntersectionObserver', FakeIntersectionObserver)
  class FakeMediaStream {
    tracks: unknown[]
    constructor(tracks: unknown[] = []) {
      this.tracks = tracks
    }
    getTracks() {
      return this.tracks
    }
    getAudioTracks() {
      return this.tracks.filter((t) => (t as { kind: string }).kind === 'audio')
    }
    getVideoTracks() {
      return this.tracks.filter((t) => (t as { kind: string }).kind === 'video')
    }
  }
  vi.stubGlobal('MediaStream', FakeMediaStream)
  vi.spyOn(console, 'log').mockImplementation(() => {})
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})

afterEach(() => {
  useConferenceStore().leaveRoom()
  vi.useRealTimers()
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

describe('conference store — simulcast (FR-127 P2)', () => {
  it("publishes the server's ladder as the camera's encodings, lowest rung first, and tells it the capture size", async () => {
    const store = await join({ view: true, audio_dtx: true, simulcast: ladder })
    expect(store.meeting.simulcast).toEqual(expectedEncodings)
    camera(640, 480)
    await store.produceLocalMedia()

    const [video] = videoProduce()
    expect(video.encodings).toEqual(expectedEncodings)
    expect(video.appData).toEqual({ source: 'camera', capture: { w: 640, h: 480 } })
    // The copy mediasoup-client gets is not the store's own ladder.
    expect(video.encodings).not.toBe(store.meeting.simulcast)
    const audio = media.produced.find((p) => (p.track as { kind: string }).kind === 'audio')
    expect(audio?.encodings).toBeUndefined()

    const onWire = produces().map((m) => m.data)
    expect(onWire.find((d) => d.kind === 'video')).toMatchObject({
      room_id: 'r1',
      source: 'camera',
      capture: { w: 640, h: 480 },
    })
    expect(onWire.find((d) => d.kind === 'audio')).not.toHaveProperty('capture')
  })

  it('publishes one encoding when no ladder is announced (null, an older server) or it cannot be read', async () => {
    for (const meeting of [
      { view: true, audio_dtx: true, simulcast: null },
      { view: true, audio_dtx: true },
      undefined,
      { view: true, audio_dtx: true, simulcast: { encodings: [ladder.encodings[1]] } },
    ]) {
      setActivePinia(createPinia())
      media.produced.length = 0
      const store = await join(meeting)
      expect(store.meeting.simulcast).toBeNull()
      camera()
      await store.produceLocalMedia()
      const [video] = videoProduce()
      expect(video.encodings).toBeUndefined()
      expect(video.appData).toEqual({ source: 'camera', capture: { w: 640, h: 480 } })
      useConferenceStore().leaveRoom()
    }
  })

  it('a ladder the browser refuses is retried as one encoding, so the camera still publishes', async () => {
    media.refuseEncodings = true
    const store = await join({ view: true, audio_dtx: true, simulcast: ladder })
    camera()
    await store.produceLocalMedia()
    const attempts = videoProduce()
    expect(attempts).toHaveLength(2)
    expect(attempts[0].encodings).toEqual(expectedEncodings)
    expect(attempts[1].encodings).toBeUndefined()
    expect(store.producers.get('video')?.id).toBe('p-1')
    expect(console.warn).toHaveBeenCalled()
  })

  it('a camera that fails without a ladder fails as before: nothing to fall back to', async () => {
    const store = await join({ view: true, audio_dtx: true, simulcast: null })
    camera()
    const transportProduce = store.sendTransport!.produce as unknown as (o: Record<string, unknown>) => Promise<unknown>
    store.sendTransport!.produce = (async (o: Record<string, unknown>) => {
      if ((o.track as { kind: string }).kind === 'video') throw new Error('NotReadableError')
      return transportProduce(o)
    }) as never
    await expect(store.produceLocalMedia()).rejects.toThrow('NotReadableError')
  })

  it('reports the tiles when only the ladder is on (pausing off), with each size for the layer map', async () => {
    const store = await join({ view: false, audio_dtx: true, simulcast: ladder })
    expect(store.meeting.view).toBe(false)
    await peerVideo('c1', 'vp-1')
    store.reportTileVisible('c1', { visible: true, w: 160, h: 120 })
    vi.advanceTimersByTime(100)
    const sent = views()
    expect(sent).toHaveLength(1)
    expect(sent[0].data.video).toEqual([{ producer_id: 'vp-1', w: 160, h: 120 }])
  })

  it('reports nothing when neither lever is on', async () => {
    const store = await join({ view: false, audio_dtx: true, simulcast: null })
    await peerVideo('c1', 'vp-1')
    store.reportTileVisible('c1', { visible: true, w: 160, h: 120 })
    vi.advanceTimersByTime(200)
    expect(views()).toEqual([])
  })

  it('marks a spotlit tile, and a resize moves the size without counting the tile twice', async () => {
    const store = await join({ view: true, audio_dtx: true, simulcast: ladder })
    await peerVideo('c1', 'vp-1')
    store.reportTileVisible('c1', { visible: true, w: 1280, h: 720, spotlit: true })
    vi.advanceTimersByTime(100)
    expect(views().at(-1)?.data.video).toEqual([{ producer_id: 'vp-1', w: 1280, h: 720, spotlit: true }])

    // The tile shrinks into the strip: the size moves, `spotlit` goes.
    store.reportTileVisible('c1', { visible: true, w: 200, h: 112, resized: true })
    vi.advanceTimersByTime(100)
    expect(views().at(-1)?.data.video).toEqual([{ producer_id: 'vp-1', w: 200, h: 112 }])

    // ONE off-screen report takes it out of the view: the resize was never a
    // second reporter.
    store.reportTileVisible('c1', { visible: false, w: 0, h: 0 })
    vi.advanceTimersByTime(100)
    expect(views().at(-1)?.data.video).toEqual([])
    expect(store.isTileOnScreen('c1')).toBe(false)

    // A resize of a tile that is not on screen is nothing.
    store.reportTileVisible('c1', { visible: true, w: 300, h: 200, resized: true })
    vi.advanceTimersByTime(100)
    expect(store.isTileOnScreen('c1')).toBe(false)
  })

  it('forgets the ladder on leave', async () => {
    const store = await join({ view: true, audio_dtx: true, simulcast: ladder })
    expect(store.meeting.simulcast).not.toBeNull()
    store.leaveRoom()
    expect(store.meeting.simulcast).toBeNull()
  })
})
