// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-135 R3 — one data-channel worker for every codec (`rc-dc-worker.ts`).
 *
 * Black-box, in the manner of `rcDecodeRecovery`: the real worker is driven
 * through the messages the composable sends it, once per codec it decodes
 * (`dcCodecs.ts`), with WebCodecs and the canvas replaced by recording fakes.
 * What R3 merged must stay as each worker had it: the codec the composable
 * names, the stats message and its keys, and the crop handling the HEVC worker
 * had, which every codec now gets.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { DC_CODECS } from './dcCodecs'

type DecoderInit = { output: (frame: unknown) => void; error: (err: unknown) => void }

class FakeDecoder {
  static instances: FakeDecoder[] = []
  static isConfigSupported = vi.fn(async (_config: VideoDecoderConfig) => ({ supported: true }))
  readonly configs: VideoDecoderConfig[] = []
  readonly chunks: { type: string; timestamp: number }[] = []
  state: 'unconfigured' | 'configured' | 'closed' = 'unconfigured'
  decodeQueueSize = 0
  constructor(readonly init: DecoderInit) {
    FakeDecoder.instances.push(this)
  }
  configure(config: VideoDecoderConfig): void {
    this.configs.push(config)
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

type Rect = { x: number; y: number; width: number; height: number }

/** A decoded frame with the geometry a decoder reported. */
class FakeFrame {
  closed = false
  readonly visibleRect: Rect
  constructor(
    readonly timestamp: number,
    readonly codedWidth: number,
    readonly codedHeight: number,
    visible: Rect,
    readonly displayWidth = visible.width,
    readonly displayHeight = visible.height,
  ) {
    this.visibleRect = visible
  }
  readonly colorSpace = { toJSON: () => ({}) }
  close(): void {
    this.closed = true
  }
}

/** `new VideoFrame(frame, { visibleRect })`, the worker's re-wrap of a spurious
 *  crop. `refuse` makes the browser reject it. */
class FakeVideoFrame {
  static refuse = false
  static made: FakeVideoFrame[] = []
  closed = false
  readonly visibleRect: Rect
  constructor(
    readonly source: FakeFrame,
    init: { visibleRect: Rect },
  ) {
    if (FakeVideoFrame.refuse) throw new TypeError('Invalid visibleRect')
    this.visibleRect = init.visibleRect
    FakeVideoFrame.made.push(this)
  }
  close(): void {
    this.closed = true
  }
}

const rect = (width: number, height: number, x = 0, y = 0): Rect => ({ x, y, width, height })

/** The canvas the worker paints, recording each `drawImage`. */
function fakeCanvas() {
  const draws: unknown[][] = []
  return {
    draws,
    width: 0,
    height: 0,
    getContext: () => ({ drawImage: (...args: unknown[]) => draws.push(args) }),
  }
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
  onmessageerror: ((ev: unknown) => void) | null
}
const scope = self as unknown as Scope
let originalPost: Scope['postMessage']
let posted: Record<string, unknown>[] = []
let clock = 0
const ofType = (t: string) => posted.filter((m) => m.type === t)

beforeEach(() => {
  vi.resetModules()
  FakeDecoder.instances = []
  FakeDecoder.isConfigSupported.mockClear()
  FakeVideoFrame.refuse = false
  FakeVideoFrame.made = []
  vi.stubGlobal('VideoDecoder', FakeDecoder)
  vi.stubGlobal('EncodedVideoChunk', FakeChunk)
  vi.stubGlobal('VideoFrame', FakeVideoFrame)
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
  scope.onmessageerror = null
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

async function boot(init: Record<string, unknown>) {
  await import('@/workers/rc-dc-worker')
  const onmessage = scope.onmessage!
  const canvas = fakeCanvas()
  onmessage({ data: { type: 'init-canvas', canvas, ...init } })
  return { send: (msg: Record<string, unknown>) => onmessage({ data: msg }), canvas }
}

/** Every key the 1 s `stats` message carries: the composable reads them by
 *  name, and the page carries some on to the agent's live law. `queueArrMs`
 *  is R3's arrival reading, added beside `queueMs`. */
const STATS_KEYS = [
  'type', 'bitrateBps', 'fps', 'width', 'height', 'framesDecodedTotal', 'bytesReceivedTotal',
  'decodeQueueSize', 'backlogMs', 'backlogMaxMs', 'framesDroppedBacklog', 'chunkGaps',
  'chunkStragglers', 'paint', 'age', 'arrival', 'viewer', 'queueMs', 'queueArrMs', 'fwd', 'decode',
  'outGapMaxMs', 'ctxMode', 'render', 'renderW', 'renderH', 'media',
]
const MEDIA_KEYS = [
  'dec', 'paint', 'fwd', 'backlog', 'arrGap', 'gate', 'chunkGaps', 'freezes', 'painted', 'decoderHw',
]

describe('FR-135 R3 — the data-channel worker without a codec named', () => {
  it('decodes VP9 profile 1, the default it had as the VP9 worker', async () => {
    await boot({})
    expect(FakeDecoder.instances[0]!.configs[0]!.codec).toBe('vp09.01.10.08')
    expect(ofType('decoder-configured')[0]).toMatchObject({ codec: 'vp09.01.10.08' })
  })
})

describe.each(DC_CODECS)('FR-135 R3 — the data-channel worker on $name', ({ codec }) => {
  /** Only HEVC acts on the crop class (rc.100, rc.102). */
  const HEVC = codec.startsWith('hev1')

  it('configures the codec the page names, and nothing else changes with it', async () => {
    await boot({ codec })
    expect(FakeDecoder.instances).toHaveLength(1)
    expect(FakeDecoder.instances[0]!.configs).toEqual([
      { codec, optimizeForLatency: true, hardwareAcceleration: 'no-preference' },
    ])
    expect(ofType('decoder-configured')).toEqual([
      { type: 'decoder-configured', codec, pref: 'no-preference', colorSpace: null },
    ])
  })

  it('sends the stats message with every key it always had', async () => {
    const { send } = await boot({ codec })
    send({ type: 'chunk', bytes: bare(true, 1), sentAt: performance.timeOrigin + clock })
    FakeDecoder.instances[0]!.init.output(new FakeFrame(1, 1920, 1080, rect(1920, 1080)))
    clock += 1_200
    send({ type: 'chunk', bytes: bare(false, 2), sentAt: performance.timeOrigin + clock })
    const stats = ofType('stats')
    expect(stats).toHaveLength(1)
    expect(Object.keys(stats[0]!).sort()).toEqual([...STATS_KEYS].sort())
    expect(Object.keys(stats[0]!.media as object).sort()).toEqual([...MEDIA_KEYS].sort())
    expect(stats[0]).toMatchObject({ width: 1920, height: 1080, framesDecodedTotal: 1, render: '2d' })
  })

  it('paints a frame with no crop at its coded size, as before', async () => {
    const { send, canvas } = await boot({ codec })
    send({ type: 'chunk', bytes: bare(true, 1) })
    const frame = new FakeFrame(1, 1920, 1200, rect(1920, 1200))
    FakeDecoder.instances[0]!.init.output(frame)
    expect(ofType('first-frame')).toEqual([
      expect.objectContaining({ width: 1920, height: 1200, crop: 'exact', rewrapped: false }),
    ])
    expect(canvas.draws).toEqual([[frame, 0, 0, 1920, 1200]])
    expect([canvas.width, canvas.height]).toEqual([1920, 1200])
    expect(frame.closed).toBe(true)
  })

  it('trusts an alignment crop: paints and reports the visible picture', async () => {
    // QSV codes 1920×1080 as 1920×1088 and hides 8 rows of pooled-surface junk
    // (field WINHOST-F); an H.264 stream crops the same way.
    const { send, canvas } = await boot({ codec })
    send({ type: 'chunk', bytes: bare(true, 1) })
    const frame = new FakeFrame(1, 1920, 1088, rect(1920, 1080))
    FakeDecoder.instances[0]!.init.output(frame)
    expect(ofType('first-frame')[0]).toMatchObject({
      width: 1920,
      height: 1080,
      coded: { w: 1920, h: 1088 },
      visible: { x: 0, y: 0, w: 1920, h: 1080 },
      crop: 'alignment',
      rewrapped: false,
    })
    expect(canvas.draws).toEqual([[frame, 0, 0, 1920, 1080]])
    expect(FakeVideoFrame.made).toEqual([])
  })

  it('re-wraps a spurious crop to the full coded picture on HEVC, and keeps the visible one elsewhere', async () => {
    // Chrome's NVDEC once said 1280×720 of a 2560×1600 desktop (field DEVBOX).
    const { send, canvas } = await boot({ codec })
    send({ type: 'chunk', bytes: bare(true, 1) })
    const frame = new FakeFrame(1, 2560, 1600, rect(1280, 720))
    FakeDecoder.instances[0]!.init.output(frame)
    if (HEVC) {
      expect(FakeVideoFrame.made).toHaveLength(1)
      const rewrap = FakeVideoFrame.made[0]!
      expect(rewrap.visibleRect).toEqual(rect(2560, 1600))
      expect(ofType('first-frame')[0]).toMatchObject({ width: 2560, height: 1600, crop: 'spurious', rewrapped: true })
      expect(canvas.draws).toEqual([[rewrap, 0, 0, 2560, 1600]])
      expect(rewrap.closed).toBe(true)
    } else {
      // The VP9 worker never re-wrapped: the decoder's visible picture, at its natural size.
      expect(FakeVideoFrame.made).toEqual([])
      expect(ofType('first-frame')[0]).toMatchObject({ width: 1280, height: 720, crop: 'spurious', rewrapped: false })
      expect(canvas.draws).toEqual([[frame, 0, 0, 1280, 720]])
    }
    expect(frame.closed).toBe(true)
  })

  it('paints the cropped frame at the coded size on HEVC when the re-wrap is refused', async () => {
    FakeVideoFrame.refuse = true
    const { send, canvas } = await boot({ codec })
    send({ type: 'chunk', bytes: bare(true, 1) })
    const frame = new FakeFrame(1, 2560, 1600, rect(1280, 720))
    FakeDecoder.instances[0]!.init.output(frame)
    const [w, h] = HEVC ? [2560, 1600] : [1280, 720]
    expect(ofType('first-frame')[0]).toMatchObject({ width: w, height: h, crop: 'spurious', rewrapped: false })
    expect(canvas.draws).toEqual([[frame, 0, 0, w, h]])
    expect(frame.closed).toBe(true)
  })

  it('paints the picture, not the padding, of a frame from Chromium’s software VP9 decoder', async () => {
    // Measured in Chromium 2026-11-07: libvpx's buffer makes the coded width
    // 64-74 px wider than the picture (1920×1080 coded 1984×1080, 1366×768
    // coded 1440×768), which HEVC's rule reads as spurious. Only HEVC acts on it.
    const { send, canvas } = await boot({ codec })
    send({ type: 'chunk', bytes: bare(true, 1) })
    const frame = new FakeFrame(1, 1984, 1080, rect(1920, 1080))
    FakeDecoder.instances[0]!.init.output(frame)
    expect(ofType('first-frame')[0]).toMatchObject({ coded: { w: 1984, h: 1080 }, crop: 'spurious' })
    if (!HEVC) {
      expect(ofType('first-frame')[0]).toMatchObject({ width: 1920, height: 1080, rewrapped: false })
      expect(canvas.draws).toEqual([[frame, 0, 0, 1920, 1080]])
      expect(FakeVideoFrame.made).toEqual([])
    }
  })

  it('re-posts the size on a change, and counts every frame after the first under the per-frame switch', async () => {
    const { send } = await boot({ codec, perFrameMsg: true })
    send({ type: 'chunk', bytes: bare(true, 1) })
    const out = FakeDecoder.instances[0]!.init.output
    out(new FakeFrame(1, 1920, 1080, rect(1920, 1080)))
    out(new FakeFrame(2, 1920, 1080, rect(1920, 1080)))
    out(new FakeFrame(3, 1280, 720, rect(1280, 720))) // the agent downscaled
    expect(ofType('first-frame').map((m) => [m.width, m.height])).toEqual([[1920, 1080], [1280, 720]])
    expect(ofType('frame-decoded').map((m) => m.count)).toEqual([2, 3])
  })
})

// ── FR-135 R3 — the channel in the worker, and the arrival reading ────────────

/** The `video-bytes` channel as the worker sees one the page moved to it. */
class FakeChannel {
  binaryType = 'blob'
  readyState: 'connecting' | 'open' | 'closing' | 'closed' = 'connecting'
  onmessage: ((ev: { data: unknown }) => void) | null = null
  onopen: (() => void) | null = null
  onclose: (() => void) | null = null
  closeCalls = 0
  close(): void {
    this.closeCalls++
  }
  deliver(data: unknown): void {
    this.onmessage?.({ data })
  }
  open(): void {
    this.readyState = 'open'
    this.onopen?.()
  }
  shut(): void {
    this.readyState = 'closed'
    this.onclose?.()
  }
}

const sum = (h: unknown) => (h as number[]).reduce((a, b) => a + b, 0)

describe.each(DC_CODECS)('FR-135 R3 — the video channel moved into the worker, on $name', ({ codec }) => {
  it('decodes off the channel, with no page hop to time and every arrival counted', async () => {
    const { send } = await boot({ codec })
    const ch = new FakeChannel()
    send({ type: 'video-channel', channel: ch })
    expect(ch.binaryType).toBe('arraybuffer')
    // The page kept no copy of the channel, and waits for this word.
    expect(ofType('video-channel-attached')).toHaveLength(1)
    ch.open()
    expect(ofType('video-channel-open')).toHaveLength(1)

    clock += 16
    ch.deliver(bare(true, 16_000))
    clock += 16
    ch.deliver(bare(false, 32_000))
    ch.deliver('a text message is no frame')
    const dec = FakeDecoder.instances[0]!
    expect(dec.chunks.map((c) => c.type)).toEqual(['key', 'delta'])
    dec.init.output(new FakeFrame(16_000, 1920, 1080, rect(1920, 1080)))
    dec.init.output(new FakeFrame(32_000, 1920, 1080, rect(1920, 1080)))
    clock += 1_200
    ch.deliver(bare(false, 1_232_000))

    const stats = ofType('stats')
    expect(stats).toHaveLength(1)
    const media = stats[0]!.media as Record<string, unknown>
    expect(sum(media.fwd), 'no page hop').toBe(0)
    expect((stats[0]!.fwd as { n: number }).n).toBe(0)
    expect(sum(media.arrGap)).toBe(2)
    expect(stats[0]!.framesDecodedTotal).toBe(2)
    expect(stats[0]!.bytesReceivedTotal).toBe(3 * 18)
  })

  it('says the channel closed, once, and logs a delivery gap as the page does', async () => {
    const { send } = await boot({ codec })
    const ch = new FakeChannel()
    send({ type: 'video-channel', channel: ch })
    ch.deliver(bare(true, 1))
    clock += 1_600
    ch.deliver(bare(false, 2))
    expect(ofType('video-channel-gap')).toEqual([{ type: 'video-channel-gap', gapMs: 1_600 }])
    ch.shut()
    ch.shut()
    expect(ofType('video-channel-closed')).toHaveLength(1)
  })

  it('says open at once for a channel already open when it arrives', async () => {
    const { send } = await boot({ codec })
    const ch = new FakeChannel()
    ch.readyState = 'open'
    send({ type: 'video-channel', channel: ch })
    ch.open() // a late event says nothing twice
    expect(ofType('video-channel-open')).toHaveLength(1)
  })

  it('tells the page when a message could not reach it: the move the page made failed', async () => {
    await boot({ codec })
    scope.onmessageerror!({ data: null })
    expect(ofType('message-error')).toEqual([{ type: 'message-error' }])
  })

  it('stops reading the channel at close, and leaves it open, as a relayed one is', async () => {
    const { send } = await boot({ codec })
    const ch = new FakeChannel()
    send({ type: 'video-channel', channel: ch })
    send({ type: 'close' })
    expect(ch.onmessage).toBeNull()
    expect(ch.onopen).toBeNull()
    expect(ch.onclose).toBeNull()
    expect(ch.closeCalls).toBe(0)
  })

  it('still times the page hop on a relayed chunk', async () => {
    const { send } = await boot({ codec })
    send({ type: 'chunk', bytes: bare(true, 1), sentAt: performance.timeOrigin + clock - 3 })
    FakeDecoder.instances[0]!.init.output(new FakeFrame(1, 1920, 1080, rect(1920, 1080)))
    clock += 1_200
    send({ type: 'chunk', bytes: bare(false, 2), sentAt: performance.timeOrigin + clock - 3 })
    const media = ofType('stats')[0]!.media as Record<string, unknown>
    expect(sum(media.fwd)).toBe(2)
  })

  it('reads the queue at arrival beside the old reading at paint, which a slow decoder moves', async () => {
    const { send } = await boot({ codec })
    const dec = () => FakeDecoder.instances[0]!
    // Frames framed and delivered every 16 ms: the link holds steady. The
    // decoder falls 10 ms further behind with each frame.
    for (let i = 0; i < 5; i++) {
      clock = 100_000 + i * 16
      send({ type: 'chunk', bytes: bare(i === 0, 1 + i * 16_000) })
    }
    for (let i = 0; i < 5; i++) {
      clock = 100_000 + i * 16 + 20 + i * 10
      dec().init.output(new FakeFrame(1 + i * 16_000, 1920, 1080, rect(1920, 1080)))
    }
    // A sixth frame on the same cadence, past the window's second.
    clock = 101_300
    send({ type: 'chunk', bytes: bare(false, 1 + 64_000 + 1_236_000) })
    const stats = ofType('stats')
    expect(stats).toHaveLength(1)
    // At paint the decoder's 10 ms a frame reads as queue growth (4 pairs);
    // at arrival the link reads flat.
    expect(stats[0]!.queueMs).toBe(40)
    expect(stats[0]!.queueArrMs).toBe(0)
  })

  it('says nothing for the queue at arrival before two whole frames have arrived', async () => {
    const { send } = await boot({ codec })
    send({ type: 'chunk', bytes: bare(true, 1) })
    clock += 1_200
    send({ type: 'chunk', bytes: bare(false, 2).slice(0, 5) }) // a frame's first bytes only
    const stats = ofType('stats')
    expect(stats).toHaveLength(1)
    expect(stats[0]!.queueArrMs).toBeNull()
    expect(stats[0]!.queueMs).toBeNull()
  })

  it('counts a growing link at arrival whatever the decoder does', async () => {
    const { send } = await boot({ codec })
    // Framed every 16 ms, delivered every 20: the queue grows 4 ms a frame.
    for (let i = 0; i < 6; i++) {
      clock = 100_000 + i * 20
      send({ type: 'chunk', bytes: bare(i === 0, 1 + i * 16_000) })
    }
    clock = 101_200
    send({ type: 'chunk', bytes: bare(false, 2).slice(0, 5) })
    expect(ofType('stats')[0]!.queueArrMs).toBe(20)
    expect(ofType('stats')[0]!.queueMs, 'nothing painted').toBeNull()
  })
})
