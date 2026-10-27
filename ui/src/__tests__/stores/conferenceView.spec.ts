// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-127 P1 — receive what is visible, the store's half: the join declares
// `caps: ["view"]`, the server's `meeting` levers are read, the tiles on
// screen become one debounced `media:view` with an increasing `seq`, nothing
// is reported when the server did not ask, and the microphone is produced
// with Opus DTX only when the server asks for it.
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

// mediasoup-client, as far as the store drives it: transports that record
// what they were asked to produce and hand back consumers of what they were
// asked to consume.
const media = vi.hoisted(() => ({
  produced: [] as Record<string, unknown>[],
}))
vi.mock('mediasoup-client', () => {
  const transport = (id: string) => ({
    id,
    on: () => {},
    close: () => {},
    produce: async (opts: Record<string, unknown>) => {
      media.produced.push(opts)
      const kind = (opts.track as { kind: string }).kind
      return { id: `prod-${kind}`, kind, pause: () => {}, resume: () => {}, close: () => {} }
    },
    consume: async (opts: { id: string; producerId: string; kind: string }) => ({
      id: opts.id,
      producerId: opts.producerId,
      kind: opts.kind,
      track: { kind: opts.kind },
      close: () => {},
    }),
  })
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

const transportCreated = (meeting?: Record<string, unknown>) => ({
  send_transport: { id: 'send', ice_parameters: {}, ice_candidates: [], dtls_parameters: {} },
  recv_transport: { id: 'recv', ice_parameters: {}, ice_candidates: [], dtls_parameters: {} },
  ice_servers: [],
  force_relay: false,
  ...(meeting ? { meeting } : {}),
})

const views = () => wire.sent.filter((m) => m.type === 'media:view')

async function join(meeting?: Record<string, unknown>) {
  wire.answers.set('media:router_capabilities', { rtp_capabilities: {} })
  wire.answers.set('media:transport_created', transportCreated(meeting))
  const store = useConferenceStore()
  await store.joinRoom('t1', 'r1', 'Room')
  return store
}

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

beforeEach(() => {
  setActivePinia(createPinia())
  vi.useFakeTimers()
  wire.sent.length = 0
  wire.handlers.clear()
  wire.answers.clear()
  media.produced.length = 0
  // jsdom has no MediaStream; the store builds one per remote peer.
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
})

afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

describe('conference store — receive what is visible (FR-127 P1)', () => {
  it('declares caps: ["view"] on the join, reads the levers, and reports an empty view at once', async () => {
    const store = await join({ view: true, audio_dtx: true, simulcast: null })
    const joinMsg = wire.sent.find((m) => m.type === 'media:join')
    expect(joinMsg?.data).toEqual({ room_id: 'r1', caps: ['view'] })
    expect(store.meeting).toEqual({ view: true, audioDtx: true })

    expect(views()).toHaveLength(0)
    vi.advanceTimersByTime(100)
    expect(views()).toEqual([{ type: 'media:view', data: { room_id: 'r1', seq: 1, video: [] } }])
  })

  it('reports a tile on screen with its producer, once per window, with a growing seq', async () => {
    const store = await join({ view: true, audio_dtx: true, simulcast: null })
    vi.advanceTimersByTime(100) // the empty report
    await peerVideo('c1', 'vp-1') // the consume itself schedules a report

    // Two mounts and one unmount inside one window: still on screen, one send.
    store.reportTileVisible('c1', { visible: true, w: 320, h: 240 })
    store.reportTileVisible('c1', { visible: true, w: 320, h: 240 })
    store.reportTileVisible('c1', { visible: false, w: 0, h: 0 })
    expect(views()).toHaveLength(1)
    vi.advanceTimersByTime(100)
    expect(views()).toHaveLength(2)
    expect(views()[1].data).toEqual({
      room_id: 'r1',
      seq: 2,
      video: [{ producer_id: 'vp-1', w: 320, h: 240 }],
    })

    // Off screen: the next report leaves it out.
    store.reportTileVisible('c1', { visible: false, w: 0, h: 0 })
    vi.advanceTimersByTime(100)
    expect(views()[2].data).toEqual({ room_id: 'r1', seq: 3, video: [] })
  })

  it('reports again when a visible tile gains its video producer after its audio', async () => {
    const store = await join({ view: true, audio_dtx: true, simulcast: null })
    vi.advanceTimersByTime(100)
    // The tile is on screen before any video producer is known for it.
    store.reportTileVisible('c1', { visible: true, w: 320, h: 240 })
    vi.advanceTimersByTime(100)
    expect(views()[1].data.video).toEqual([])

    await peerVideo('c1', 'vp-1')
    vi.advanceTimersByTime(100)
    expect(views()[2].data.video).toEqual([{ producer_id: 'vp-1', w: 320, h: 240 }])
  })

  it('reports nothing when the server did not ask (no meeting, or view off)', async () => {
    for (const meeting of [undefined, { view: false, audio_dtx: false, simulcast: null }]) {
      setActivePinia(createPinia())
      wire.sent.length = 0
      const store = await join(meeting)
      expect(store.meeting).toEqual({ view: false, audioDtx: false })
      store.reportTileVisible('c1', { visible: true, w: 320, h: 240 })
      vi.advanceTimersByTime(500)
      expect(views()).toHaveLength(0)
    }
  })

  it('produces the microphone with Opus DTX only when the server asks for it', async () => {
    for (const [dtx, expected] of [
      [true, { opusDtx: true }],
      [false, undefined],
    ] as const) {
      setActivePinia(createPinia())
      media.produced.length = 0
      const store = await join({ view: true, audio_dtx: dtx, simulcast: null })
      const tracks = [{ kind: 'audio' }, { kind: 'video' }]
      vi.stubGlobal('navigator', {
        mediaDevices: {
          getUserMedia: async () => ({
            getAudioTracks: () => tracks.filter((t) => t.kind === 'audio'),
            getVideoTracks: () => tracks.filter((t) => t.kind === 'video'),
            getTracks: () => tracks,
          }),
        },
      })
      await store.produceLocalMedia()
      const audio = media.produced.find((p) => (p.track as { kind: string }).kind === 'audio')
      expect(audio?.codecOptions).toEqual(expected)
      const video = media.produced.find((p) => (p.track as { kind: string }).kind === 'video')
      expect(video?.codecOptions).toBeUndefined()
    }
  })

  it('forgets the view on leave, and the next call continues the seq', async () => {
    const store = await join({ view: true, audio_dtx: true, simulcast: null })
    vi.advanceTimersByTime(100)
    store.reportTileVisible('c1', { visible: true, w: 320, h: 240 })
    store.leaveRoom()
    vi.advanceTimersByTime(500)
    expect(views()).toHaveLength(1)
    expect(store.meeting).toEqual({ view: false, audioDtx: false })

    await store.joinRoom('t1', 'r1', 'Room')
    vi.advanceTimersByTime(100)
    expect(views()[1].data).toEqual({ room_id: 'r1', seq: 2, video: [] })
  })
})
