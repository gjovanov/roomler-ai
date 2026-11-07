// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-135 R0 — the decode worker's window of media numbers for the agent: the
 * buckets, the freeze and painted counts, the gate's wait, the decoder's form,
 * and both data-channel workers carrying them in their `stats` message.
 *
 * The second half is black-box, in the manner of `rcDecodeRecovery`: each real
 * DC worker is driven through the messages the composable sends it, with
 * WebCodecs replaced by a recording fake and a clock the test moves.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import {
  FREEZE_GAP_MS,
  LogHist,
  MEDIA_BUCKETS,
  createViewerMediaRecorder,
  decoderIsHardware,
  logBucket,
  type ViewerMediaWindow,
} from '@/workers/rc-media-window'

/** A taken window's samples as their buckets, lowest first. */
const only = (h: number[]) => h.flatMap((n, i) => Array<number>(n).fill(i))

describe('FR-135 R0 — the viewer’s histograms', () => {
  it('bucket a sample as the agent buckets one (media_window::LogHist)', () => {
    // The agent's own vectors, in microseconds there, ms here.
    const cases: [number, number][] = [
      [0, 0],
      [0.999, 0],
      [1, 1],
      [1.999, 1],
      [2, 2],
      [3.999, 2],
      [4, 3],
      [512, 10],
      [1023.999, 10],
      [1024, 11],
      [60_000, 11],
    ]
    for (const [ms, bucket] of cases) expect(logBucket(ms), `${ms} ms`).toBe(bucket)
  })

  it('ignore a sample no clock could give, and start over when taken', () => {
    const h = new LogHist()
    h.add(Number.NaN)
    h.add(-3)
    h.add(Number.POSITIVE_INFINITY)
    h.add(5)
    const w = h.take()
    expect(w).toHaveLength(MEDIA_BUCKETS)
    expect(only(w)).toEqual([3])
    expect(h.take()).toEqual(Array<number>(MEDIA_BUCKETS).fill(0))
  })
})

describe('FR-135 R0 — the recorder', () => {
  const take = (r: ReturnType<typeof createViewerMediaRecorder>, gaps = 0): ViewerMediaWindow => r.take(gaps)

  it('counts every painted frame, cumulative across windows, and starts over at reset', () => {
    const r = createViewerMediaRecorder(async () => null)
    r.onPainted(0, 2, 1)
    r.onPainted(16, 2, 2)
    expect(take(r).painted).toBe(2)
    r.onPainted(33, 2, 3)
    const w = take(r)
    expect(w.painted).toBe(3)
    expect(only(w.paint)).toEqual([2])
    r.reset()
    expect(take(r).painted).toBe(0)
  })

  it(`counts a freeze for a paint gap over ${FREEZE_GAP_MS} ms, and only past it`, () => {
    const r = createViewerMediaRecorder(async () => null)
    r.onPainted(1_000, 1, 1) // the first frame: no gap before it
    r.onPainted(1_000 + FREEZE_GAP_MS, 1, 2) // exactly the bar: not a freeze
    r.onPainted(1_000 + FREEZE_GAP_MS + 60, 1, 3) // a keepalive's cadence
    r.onPainted(1_000 + FREEZE_GAP_MS + 60 + 201, 1, 4) // a stall
    expect(take(r).freezes).toBe(1)
    r.onPainted(5_000, 1, 5)
    expect(take(r).freezes).toBe(2)
    r.reset()
    r.onPainted(9_000, 1, 6)
    expect(take(r).freezes).toBe(0)
  })

  it('times the gate from its first arming to the painted key frame, after the first paint only', () => {
    const r = createViewerMediaRecorder(async () => null)
    // Before the first paint the gate is the time to first frame: not timed.
    r.onGateArmed(0)
    r.onGateOpened(10)
    r.onPainted(50, 1, 10)
    expect(only(take(r).gate)).toEqual([])

    r.onGateArmed(100)
    r.onGateArmed(150) // armed again while it waits: the picture stopped at 100
    // A frame the decoder still held when the gate closed paints first.
    r.onPainted(160, 1, 11)
    r.onGateOpened(20)
    r.onPainted(400, 1, 20) // the key frame: 300 ms after the first arming
    r.onPainted(420, 1, 21) // and an ordinary frame after it
    expect(only(take(r).gate)).toEqual([logBucket(300)])
  })

  it('waits for the next key frame when the gate closes again before its key painted', () => {
    const r = createViewerMediaRecorder(async () => null)
    r.onPainted(0, 1, 1)
    r.onGateArmed(100)
    r.onGateOpened(30)
    r.onGateArmed(120) // the key frame's decoder failed: a new one waits again
    r.onPainted(130, 1, 30) // the failed key frame's paint is not the recovery
    expect(only(take(r).gate)).toEqual([])
    r.onGateOpened(40)
    r.onPainted(700, 1, 40)
    expect(only(take(r).gate)).toEqual([logBucket(600)])
  })

  it('folds the window’s samples and keeps the chunk gaps it is given', () => {
    const r = createViewerMediaRecorder(async () => null)
    r.onArrival(0)
    r.onArrival(16)
    r.onArrival(250)
    r.onForward(0.4)
    r.onBacklog(3)
    r.onDecoded(9)
    const w = take(r, 7)
    expect(only(w.arrGap)).toEqual([logBucket(16), logBucket(234)])
    expect(only(w.fwd)).toEqual([0])
    expect(only(w.backlog)).toEqual([logBucket(3)])
    expect(only(w.dec)).toEqual([logBucket(9)])
    expect(w.chunkGaps).toBe(7)
    expect(only(take(r).arrGap), 'a fresh window').toEqual([])
  })

  it('reports the decoder’s form once the browser answers, and drops a stale answer', async () => {
    let answer!: (hw: boolean | null) => void
    const asked: string[] = []
    const r = createViewerMediaRecorder((codec, pref) => {
      asked.push(`${codec}:${pref}`)
      return new Promise((res) => (answer = res))
    })
    expect(take(r).decoderHw).toBeNull()
    r.probeDecoderHw('vp09.00.10.08', 'no-preference')
    const first = answer
    r.probeDecoderHw('vp09.00.10.08', 'no-preference') // a rebuilt decoder asks again
    first(false)
    await Promise.resolve()
    expect(take(r).decoderHw, 'the earlier ask’s answer').toBeNull()
    answer(true)
    await Promise.resolve()
    expect(take(r).decoderHw).toBe(true)
    r.reset()
    expect(take(r).decoderHw).toBeNull()
    expect(asked).toEqual(['vp09.00.10.08:no-preference', 'vp09.00.10.08:no-preference'])
  })
})

describe('FR-135 R0 — what the browser says about the decoder', () => {
  afterEach(() => vi.unstubAllGlobals())

  it('is software when the session forces it, without asking', async () => {
    const isConfigSupported = vi.fn()
    vi.stubGlobal('VideoDecoder', { isConfigSupported })
    expect(await decoderIsHardware('hev1.1.6.L153.B0', 'prefer-software')).toBe(false)
    expect(isConfigSupported).not.toHaveBeenCalled()
  })

  it('asks for the same codec with prefer-hardware, and takes the answer', async () => {
    const isConfigSupported = vi.fn(async (c: VideoDecoderConfig) => ({ supported: c.codec.startsWith('hev1') }))
    vi.stubGlobal('VideoDecoder', { isConfigSupported })
    expect(await decoderIsHardware('hev1.1.6.L153.B0', 'no-preference')).toBe(true)
    expect(await decoderIsHardware('vp09.01.10.08', 'prefer-hardware')).toBe(false)
    expect(isConfigSupported.mock.calls[0]![0]).toEqual({
      codec: 'hev1.1.6.L153.B0',
      hardwareAcceleration: 'prefer-hardware',
      optimizeForLatency: true,
    })
  })

  it('does not know when the browser cannot say', async () => {
    vi.stubGlobal('VideoDecoder', undefined)
    expect(await decoderIsHardware('vp8', 'no-preference')).toBeNull()
    vi.stubGlobal('VideoDecoder', { isConfigSupported: async () => { throw new TypeError('bad config') } })
    expect(await decoderIsHardware('vp8', 'no-preference')).toBeNull()
    vi.stubGlobal('VideoDecoder', { isConfigSupported: async () => ({}) })
    expect(await decoderIsHardware('vp8', 'no-preference')).toBeNull()
  })
})

// ── both data-channel workers, black-box ──────────────────────────────────────

type DecoderInit = { output: (frame: unknown) => void; error: (err: unknown) => void }

class FakeDecoder {
  static instances: FakeDecoder[] = []
  static hardware = true
  static isConfigSupported = vi.fn(async (_config: VideoDecoderConfig) => ({ supported: FakeDecoder.hardware }))
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
    if (this.state === 'closed') throw new DOMException('closed', 'InvalidStateError')
    this.chunks.push(chunk)
  }
  close(): void {
    this.state = 'closed'
  }
  fail(err: unknown): void {
    this.state = 'closed'
    this.init.error(err)
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

function fakeFrame(timestamp: number) {
  return {
    timestamp,
    displayWidth: 1920,
    displayHeight: 1080,
    codedWidth: 1920,
    codedHeight: 1080,
    visibleRect: { x: 0, y: 0, width: 1920, height: 1080 },
    colorSpace: { toJSON: () => ({}) },
    close: () => {},
  }
}

function fakeCanvas() {
  return { width: 0, height: 0, getContext: () => ({ drawImage: () => {} }) }
}

/** One frame as the agent frames it: u32 size, u8 flags (bit 0: key), u64
 *  timestamp, payload. */
function bare(key: boolean, timestampUs: number): ArrayBuffer {
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
}
const scope = self as unknown as Scope
let originalPost: Scope['postMessage']
let posted: Record<string, unknown>[] = []
let clock = 0

beforeEach(() => {
  vi.resetModules()
  FakeDecoder.instances = []
  FakeDecoder.hardware = true
  FakeDecoder.isConfigSupported.mockClear()
  vi.stubGlobal('VideoDecoder', FakeDecoder)
  vi.stubGlobal('EncodedVideoChunk', FakeChunk)
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
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

const DC_WORKERS = [
  { name: 'rc-vp9-444-worker', load: () => import('@/workers/rc-vp9-444-worker') },
  { name: 'rc-hevc-worker', load: () => import('@/workers/rc-hevc-worker') },
] as const

describe.each(DC_WORKERS)('FR-135 R0 — $name carries the window in its stats', ({ load }) => {
  async function start(init: Record<string, unknown> = {}) {
    await load()
    const onmessage = scope.onmessage!
    onmessage({ data: { type: 'init-canvas', canvas: fakeCanvas(), ...init } })
    return (msg: Record<string, unknown>) => onmessage({ data: msg })
  }

  /** At `t` ms after the start, a chunk carrying one whole frame. */
  const at = (t: number) => {
    clock = 100_000 + t
  }

  /** The `media` of the `stats` message a frame at `t` sends out, once the
   *  decoder probe has had its answer. */
  async function statsAfter(send: (m: Record<string, unknown>) => void, t: number, ts: number) {
    await new Promise((r) => setTimeout(r, 0))
    at(t)
    send({ type: 'chunk', bytes: bare(false, ts) })
    const stats = posted.filter((m) => m.type === 'stats')
    expect(stats.length).toBeGreaterThan(0)
    return stats.at(-1)!.media as ViewerMediaWindow
  }

  it('counts painted frames and freezes, times the backlog gate to the key frame, and names the decoder', async () => {
    const send = await start()
    const dec = () => FakeDecoder.instances[0]!
    at(0)
    send({ type: 'chunk', bytes: bare(true, 1) })
    at(10)
    dec().init.output(fakeFrame(1))
    // A delta the decoder sits on, then one that arrives behind it: the backlog
    // passes the drop threshold, the delta is dropped and the gate closes.
    at(20)
    send({ type: 'chunk', bytes: bare(false, 2) })
    at(200)
    send({ type: 'chunk', bytes: bare(false, 3) })
    expect(posted.filter((m) => m.type === 'backlog-drop')).toHaveLength(1)
    // The held delta paints: 200 ms after the last paint, not a freeze, and
    // not the recovery either.
    at(210)
    dec().init.output(fakeFrame(2))
    // The key frame through the gate, painted 250 ms after it closed.
    at(400)
    send({ type: 'chunk', bytes: bare(true, 4) })
    at(450)
    dec().init.output(fakeFrame(4))

    const media = await statsAfter(send, 1_500, 5)
    expect(media.painted).toBe(3)
    expect(media.freezes).toBe(1)
    expect(only(media.gate)).toEqual([logBucket(250)])
    expect(only(media.dec)).toEqual([logBucket(10), logBucket(50), logBucket(190)])
    expect(media.paint.reduce((a, b) => a + b, 0)).toBe(3)
    expect(only(media.arrGap)).toEqual([logBucket(20), logBucket(180), logBucket(200), logBucket(1_100)])
    // One sample per frame the gate let through, the dropped delta included.
    expect(only(media.backlog)).toEqual([0, 0, 0, 0, logBucket(180)])
    expect(media.chunkGaps).toBe(0)
    expect(media.decoderHw).toBe(true)
    expect(FakeDecoder.isConfigSupported).toHaveBeenCalledWith(
      expect.objectContaining({ hardwareAcceleration: 'prefer-hardware' }),
    )
  })

  it('times an R1 recovery as a gate wait, from the failure to the painted key frame', async () => {
    const send = await start({ decodeRecover: true })
    at(0)
    send({ type: 'chunk', bytes: bare(true, 1) })
    at(10)
    FakeDecoder.instances[0]!.init.output(fakeFrame(1))
    at(100)
    FakeDecoder.instances[0]!.fail(new Error('Decoding error.'))
    expect(FakeDecoder.instances).toHaveLength(2)
    at(150)
    send({ type: 'chunk', bytes: bare(false, 2) }) // waits at the gate
    at(300)
    send({ type: 'chunk', bytes: bare(true, 3) })
    at(340)
    FakeDecoder.instances[1]!.init.output(fakeFrame(3))
    expect(posted.filter((m) => m.type === 'decode-recovered')).toEqual([
      { type: 'decode-recovered', ms: 240, attempts: 1 },
    ])

    const media = await statsAfter(send, 1_500, 4)
    expect(only(media.gate)).toEqual([logBucket(240)])
    expect(media.freezes).toBe(1)
    expect(media.painted).toBe(2)
  })

  it('says software under prefer-software, and the window is a fresh one each second', async () => {
    const send = await start({ decodePref: 'prefer-software' })
    at(0)
    send({ type: 'chunk', bytes: bare(true, 1) })
    at(5)
    FakeDecoder.instances[0]!.init.output(fakeFrame(1))
    const first = await statsAfter(send, 1_200, 2)
    expect(first.decoderHw).toBe(false)
    expect(FakeDecoder.isConfigSupported).not.toHaveBeenCalled()
    expect(first.painted).toBe(1)
    const second = await statsAfter(send, 2_400, 3)
    expect(second.painted, 'cumulative').toBe(1)
    expect(second.dec.reduce((a, b) => a + b, 0), 'a fresh window').toBe(0)
  })
})
