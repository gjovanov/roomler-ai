// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-138 P1c (#3604) — both decode workers answer the page's `paint-gap-mark`:
 * at once with how long ago they last painted, and at the first frame painted
 * after it with the gap across the mark, both on the worker's own clock. That
 * is `rc.resume`'s `gap_ms` on the data channel and on the RTP transform.
 *
 * Black-box, as `rcDcWorker.spec.ts` drives them: the real worker module, the
 * messages the composable sends it, WebCodecs and the canvas faked.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

type DecoderInit = { output: (frame: unknown) => void; error: (err: unknown) => void }

class FakeDecoder {
  static instances: FakeDecoder[] = []
  static isConfigSupported = vi.fn(async (config: VideoDecoderConfig) => ({ supported: true, config }))
  readonly chunks: { type: string; timestamp: number }[] = []
  state: 'unconfigured' | 'configured' | 'closed' = 'unconfigured'
  decodeQueueSize = 0
  constructor(readonly init: DecoderInit) {
    FakeDecoder.instances.push(this)
  }
  configure(): void {
    this.state = 'configured'
  }
  decode(chunk: { type: string; timestamp: number }): void {
    this.chunks.push(chunk)
  }
  close(): void {
    this.state = 'closed'
  }
}

class FakeChunk {
  type: string
  timestamp: number
  constructor(init: { type: string; timestamp: number }) {
    this.type = init.type
    this.timestamp = init.timestamp
  }
}

function frame(timestamp: number) {
  return {
    timestamp,
    displayWidth: 1920,
    displayHeight: 1080,
    codedWidth: 1920,
    codedHeight: 1080,
    visibleRect: { x: 0, y: 0, width: 1920, height: 1080 },
    colorSpace: { toJSON: () => ({}) },
    clone() {
      return frame(timestamp)
    },
    close: () => {},
  }
}

function fakeCanvas() {
  return { width: 0, height: 0, getContext: () => ({ drawImage: () => {} }) }
}

/** One frame on the data channel: u32 size, u8 flags (bit 0: key), u64 timestamp, payload. */
function dcFrame(key: boolean, timestampUs: number): ArrayBuffer {
  const payload = [0, 0, 0, 1, 0x65]
  const buf = new ArrayBuffer(13 + payload.length)
  const view = new DataView(buf)
  view.setUint32(0, payload.length, true)
  view.setUint8(4, key ? 1 : 0)
  view.setUint32(5, timestampUs, true)
  view.setUint32(9, 0, true)
  new Uint8Array(buf, 13).set(payload)
  return buf
}

type Scope = {
  postMessage: (msg: unknown) => void
  onmessage: ((ev: { data: unknown }) => void) | null
  onrtctransform: ((ev: unknown) => Promise<void>) | null
}
const scope = self as unknown as Scope
let originalPost: Scope['postMessage']
let posted: Record<string, unknown>[] = []
let clock = 0
const ofType = (t: string) => posted.filter((m) => m.type === t)

beforeEach(() => {
  vi.resetModules()
  FakeDecoder.instances = []
  vi.stubGlobal('VideoDecoder', FakeDecoder)
  vi.stubGlobal('EncodedVideoChunk', FakeChunk)
  // No rAF: the DC worker's first frame starts its viewer-rate bursts
  // (`rc-viewer-hz.ts`), whose late report would land after this file
  // restores `postMessage`.
  vi.stubGlobal('requestAnimationFrame', undefined)
  clock = 100_000
  vi.spyOn(performance, 'now').mockImplementation(() => clock)
  posted = []
  originalPost = scope.postMessage
  scope.postMessage = (msg) => {
    posted.push(msg as Record<string, unknown>)
  }
})

afterEach(() => {
  scope.postMessage = originalPost
  scope.onmessage = null
  scope.onrtctransform = null
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

describe('FR-138 P1c — rc-dc-worker answers the paint-gap mark', () => {
  async function boot() {
    await import('@/workers/rc-dc-worker')
    const onmessage = scope.onmessage!
    onmessage({ data: { type: 'init-canvas', canvas: fakeCanvas() } })
    const send = (msg: Record<string, unknown>) => onmessage({ data: msg })
    let ts = 0
    const paintOne = () => {
      ts += 1
      send({ type: 'chunk', bytes: dcFrame(ts === 1, ts) })
      FakeDecoder.instances[0]!.init.output(frame(ts))
    }
    return { send, paintOne }
  }

  it('says how long ago it painted, then the gap at the first frame painted after the mark, once', async () => {
    const { send, paintOne } = await boot()
    paintOne()
    clock += 50
    send({ type: 'paint-gap-mark' })
    expect(ofType('paint-gap-marked')).toEqual([{ type: 'paint-gap-marked', sinceMs: 50 }])
    expect(ofType('paint-gap')).toEqual([])
    clock += 30
    paintOne()
    expect(ofType('paint-gap')).toEqual([{ type: 'paint-gap', gapMs: 80 }])
    clock += 16
    paintOne()
    expect(ofType('paint-gap')).toHaveLength(1)
  })

  it('a mark before any frame: no time since, and no gap to name', async () => {
    const { send, paintOne } = await boot()
    send({ type: 'paint-gap-mark' })
    expect(ofType('paint-gap-marked')).toEqual([{ type: 'paint-gap-marked', sinceMs: null }])
    clock += 40
    paintOne()
    expect(ofType('paint-gap')).toEqual([{ type: 'paint-gap', gapMs: null }])
  })

  it('says nothing about its frames without a mark', async () => {
    const { paintOne } = await boot()
    for (let i = 0; i < 5; i += 1) {
      clock += 16
      paintOne()
    }
    expect(ofType('paint-gap')).toEqual([])
    expect(ofType('paint-gap-marked')).toEqual([])
  })
})

describe('FR-138 P1c — rc-webcodecs-worker answers the paint-gap mark', () => {
  async function start() {
    await import('@/workers/rc-webcodecs-worker')
    const onmessage = scope.onmessage!
    onmessage({ data: { type: 'init-canvas', canvas: fakeCanvas() } })
    const readable = new ReadableStream<unknown>({ start() {} })
    const writable = new WritableStream<unknown>({ write() {} })
    await scope.onrtctransform!({ transformer: { readable, writable, options: { codec: 'h264' } } })
    const send = (msg: Record<string, unknown>) => onmessage({ data: msg })
    let ts = 0
    const paintOne = () => {
      ts += 1
      FakeDecoder.instances[0]!.init.output(frame(ts))
    }
    return { send, paintOne }
  }

  it('says how long ago it painted, then the gap at the first frame painted after the mark, once', async () => {
    const { send, paintOne } = await start()
    paintOne()
    clock += 25
    send({ type: 'paint-gap-mark' })
    expect(ofType('paint-gap-marked')).toEqual([{ type: 'paint-gap-marked', sinceMs: 25 }])
    clock += 2_411
    paintOne()
    expect(ofType('paint-gap')).toEqual([{ type: 'paint-gap', gapMs: 2_436 }])
    clock += 16
    paintOne()
    expect(ofType('paint-gap')).toHaveLength(1)
  })
})
