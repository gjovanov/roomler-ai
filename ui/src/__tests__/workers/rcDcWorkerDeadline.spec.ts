// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-148 P4b-2 (D55) — the data-channel worker reads two video channels:
 * `video-bytes`, reliable and ordered, and `video-deadline`, unordered with no
 * retransmits. The agent will send each frame on one of them, keep one
 * frame_seq space across both and switch only at a key frame (P4b-3), so the
 * worker feeds both into its one chunk assembler, keyed by frame_seq.
 *
 * Black-box, in the manner of `rcDcWorker.spec.ts`: the real worker, driven
 * through the messages the page sends it, with WebCodecs replaced by recording
 * fakes. Each cell runs both ways the page can deliver the channels: relayed
 * (`chunk`, with `deadline: true` on a message off `video-deadline`) and moved
 * into the worker (`video-channel`, FR-135 R3). One codec: the chunk path does
 * not look at it (`rcDcWorker.spec.ts` covers every codec).
 *
 * The cells are the brief's: 1 order across the channels, 2 stragglers, 3 one
 * key-frame ask per gap, 4 an old agent (nothing on `video-deadline`).
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { agentChunks, oldAgentStream, wireFrame, type StreamFrame } from './dcFrames'

type DecoderInit = { output: (frame: unknown) => void; error: (err: unknown) => void }
type Decoded = { type: string; timestamp: number; data: Uint8Array }

class FakeDecoder {
  static instances: FakeDecoder[] = []
  static isConfigSupported = vi.fn(async (_config: VideoDecoderConfig) => ({ supported: true }))
  readonly chunks: Decoded[] = []
  state: 'unconfigured' | 'configured' | 'closed' = 'unconfigured'
  decodeQueueSize = 0
  constructor(readonly init: DecoderInit) {
    FakeDecoder.instances.push(this)
  }
  configure(_config: VideoDecoderConfig): void {
    this.state = 'configured'
  }
  decode(chunk: Decoded): void {
    this.chunks.push(chunk)
  }
  close(): void {
    this.state = 'closed'
  }
}

class FakeChunk {
  type: string
  timestamp: number
  data: Uint8Array
  constructor(init: Decoded) {
    this.type = init.type
    this.timestamp = init.timestamp
    this.data = init.data
  }
}

/** A channel as the worker sees one the page moved to it. */
class FakeChannel {
  binaryType = 'blob'
  readyState: 'connecting' | 'open' | 'closing' | 'closed' = 'connecting'
  onmessage: ((ev: { data: unknown }) => void) | null = null
  onopen: (() => void) | null = null
  onclose: (() => void) | null = null
  deliver(data: unknown): void {
    this.onmessage?.({ data })
  }
  open(): void {
    this.readyState = 'open'
    this.onopen?.()
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

type Scope = {
  postMessage: (msg: unknown) => void
  onmessage: ((ev: { data: unknown }) => void) | null
  onmessageerror: ((ev: unknown) => void) | null
}
const scope = self as unknown as Scope
let originalPost: Scope['postMessage']
let posted: Record<string, unknown>[] = []
let clock = 0
/** How many of the decoder's chunks have come out of it. */
let painted = 0
const ofType = (t: string) => posted.filter((m) => m.type === t)

beforeEach(() => {
  vi.resetModules()
  FakeDecoder.instances = []
  vi.stubGlobal('VideoDecoder', FakeDecoder)
  vi.stubGlobal('EncodedVideoChunk', FakeChunk)
  // No rAF: the first frame starts the viewer-rate bursts, whose late report
  // would land after this file restores `postMessage` (`rcDcWorker.spec.ts`).
  vi.stubGlobal('requestAnimationFrame', undefined)
  clock = 100_000
  painted = 0
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
  onmessage({ data: { type: 'init-canvas', canvas: fakeCanvas(), ...init } })
  return (msg: Record<string, unknown>) => onmessage({ data: msg })
}

/** The decoder puts out every frame it was given, at once. */
function paintAll(): void {
  const dec = FakeDecoder.instances.at(-1)!
  while (painted < dec.chunks.length) dec.init.output(fakeFrame(dec.chunks[painted++]!.timestamp))
}

const decoded = () => FakeDecoder.instances[0]!.chunks.map((c) => [c.type, c.timestamp])

const RELAYED = 'relayed by the page'
const MOVED = 'moved into the worker'
const MODES = [RELAYED, MOVED] as const
type Mode = (typeof MODES)[number]

interface Session {
  /** One message off `video-bytes`. */
  bytes: (buf: ArrayBuffer) => void
  /** One message off `video-deadline`. */
  deadline: (buf: ArrayBuffer) => void
}

/** A framed session whose two channels reach the worker the `mode` way. The
 *  session defaults are the page's: framing negotiated, recovery on. The
 *  decoder keeps up (each message's frames come out at once), so the backlog
 *  shed, which also closes the gate and asks, stays out of every cell. */
async function session(mode: Mode): Promise<Session> {
  const send = await boot({ chunkFraming: true, decodeRecover: true })
  const thenPaint = (deliver: (buf: ArrayBuffer) => void) => (buf: ArrayBuffer) => {
    deliver(buf)
    paintAll()
  }
  if (mode === RELAYED) {
    const sentAt = () => performance.timeOrigin + clock
    return {
      bytes: thenPaint((buf) => send({ type: 'chunk', bytes: buf, sentAt: sentAt() })),
      deadline: thenPaint((buf) => send({ type: 'chunk', bytes: buf, sentAt: sentAt(), deadline: true })),
    }
  }
  const bytesCh = new FakeChannel()
  const deadlineCh = new FakeChannel()
  send({ type: 'video-channel', channel: bytesCh, deadline: deadlineCh })
  bytesCh.open()
  deadlineCh.open()
  return {
    bytes: thenPaint((buf) => bytesCh.deliver(buf)),
    deadline: thenPaint((buf) => deadlineCh.deliver(buf)),
  }
}

const CHANNELS = ['video-bytes', 'video-deadline'] as const
type Channel = (typeof CHANNELS)[number]
const on = (s: Session, ch: Channel) => (ch === 'video-bytes' ? s.bytes : s.deadline)

describe.each(MODES)('FR-148 P4b-2 — two video channels into one assembler, %s', (mode) => {
  it('cell 1: assembles frames whose chunks interleave across the two channels, by frame_seq', async () => {
    const s = await session(mode)
    // Frames 1-3 on video-bytes; the switch's key frame 4 and its deltas 5-6 on
    // video-deadline; the switch back at key frame 7, and 8 behind it.
    const frames = [
      wireFrame(true, 1_000, 40_000),
      wireFrame(false, 2_000, 900),
      wireFrame(false, 3_000, 20_000),
      wireFrame(true, 4_000, 40_000),
      wireFrame(false, 5_000, 20_000),
      wireFrame(false, 6_000, 700),
      wireFrame(true, 7_000, 20_000),
      wireFrame(false, 8_000, 800),
    ]
    const chunks = frames.map((f, i) => agentChunks(i + 1, f))
    const channelOf = (seq: number): Channel => (seq >= 4 && seq <= 6 ? 'video-deadline' : 'video-bytes')
    // The two channels race each other, and a frame's chunks on the unordered
    // one arrive in any order.
    const arrival: [number, number][] = [
      [1, 0], [1, 1], [1, 2], [2, 0], [3, 0], [4, 1], [3, 1], [4, 0], [5, 1], [4, 2], [5, 0],
      [7, 0], [6, 0], [7, 1], [8, 0],
    ]
    for (const [seq, idx] of arrival) {
      clock += 1
      on(s, channelOf(seq))(chunks[seq - 1]![idx]!)
    }
    paintAll()

    expect(decoded()).toEqual([
      ['key', 1_000], ['delta', 2_000], ['delta', 3_000], ['key', 4_000],
      ['delta', 5_000], ['delta', 6_000], ['key', 7_000], ['delta', 8_000],
    ])
    // Each frame whole, its own bytes and no other frame's.
    const dec = FakeDecoder.instances[0]!
    dec.chunks.forEach((c, i) => expect(c.data, `frame ${i + 1}`).toEqual(frames[i]!.subarray(13)))
    expect(ofType('frame-rejected')).toEqual([])
    expect(ofType('request-keyframe')).toEqual([])
    expect(ofType('awaiting-keyframe')).toEqual([])
  })

  it.each([
    ['video-deadline', 'video-bytes'],
    ['video-bytes', 'video-deadline'],
  ] as const)(
    'cell 2: a key frame whole on %s drops the frame still partial on %s as a straggler, and asks for nothing',
    async (keyOn, oldOn) => {
      const s = await session(mode)
      const frames = [
        wireFrame(true, 1_000, 900),
        wireFrame(false, 2_000, 900),
        wireFrame(false, 3_000, 20_000), // two chunks: in flight on the old channel at the switch
        wireFrame(true, 4_000, 40_000), // the switch's key frame, three chunks
        wireFrame(false, 5_000, 900),
      ]
      const c = frames.map((f, i) => agentChunks(i + 1, f))
      on(s, oldOn)(c[0]![0]!)
      on(s, oldOn)(c[1]![0]!)
      paintAll()
      clock += 1_000 // past the resync debounce: an ask here would go out
      posted = []

      on(s, oldOn)(c[2]![0]!) // frame 3's first chunk; then the agent switches
      for (const m of c[3]!) on(s, keyOn)(m) // the key frame, whole, on the other channel
      on(s, oldOn)(c[2]![1]!) // frame 3's last chunk, late
      on(s, keyOn)(c[4]![0]!) // a delta behind the key frame
      paintAll()

      expect(ofType('frame-rejected')).toEqual([])
      expect(ofType('request-keyframe')).toEqual([])
      expect(decoded()).toEqual([['key', 1_000], ['delta', 2_000], ['key', 4_000], ['delta', 5_000]])
      // Counted where the stats put chunks of a frame moved past, not as loss:
      // the chunk frame 3 held when the key frame completed, and its late one.
      clock += 1_200
      on(s, keyOn)(agentChunks(6, wireFrame(false, 6_000, 900))[0]!)
      const stats = ofType('stats').at(-1)!
      expect(stats.chunkGaps).toBe(0)
      expect(stats.chunkStragglers).toBe(2)
    },
  )

  it.each(CHANNELS)('cell 3: a gap revealed on %s gives one key-frame ask per debounce window', async (ch) => {
    const s = await session(mode)
    const send = on(s, ch)
    let seq = 0
    const whole = (key: boolean) => {
      seq++
      for (const m of agentChunks(seq, wireFrame(key, seq * 1_000, 900))) send(m)
    }
    /** A two-chunk frame whose second chunk never arrives. */
    const lost = () => {
      seq++
      send(agentChunks(seq, wireFrame(false, seq * 1_000, 20_000))[0]!)
    }
    whole(true)
    whole(false)
    paintAll()
    clock += 1_000 // past the resync debounce
    posted = []

    // One loss: the next frame completes over it.
    lost()
    whole(false)
    expect(ofType('frame-rejected')).toEqual([{ type: 'frame-rejected', reason: 'chunk-gap', gaps: 1 }])
    expect(ofType('request-keyframe')).toHaveLength(1)
    // A second loss and the deltas behind both, all inside the 250 ms window:
    // each gap is reported, and the window still asks once.
    clock += 50
    lost()
    whole(false)
    for (let i = 0; i < 15; i++) {
      clock += 10
      whole(false)
    }
    paintAll()
    expect(ofType('frame-rejected')).toHaveLength(2)
    expect(ofType('request-keyframe')).toHaveLength(1)

    // The next window. Behind a gap on video-bytes the gate holds the deltas
    // and asks once a window until the key frame lands (today's handling).
    // Behind one on video-deadline the deltas decode, as on the retired
    // unordered opt-in, and nothing more is asked.
    clock += 100
    whole(false)
    clock += 10
    whole(false)
    expect(ofType('request-keyframe')).toHaveLength(ch === 'video-bytes' ? 2 : 1)
    clock += 300
    whole(false)
    expect(ofType('request-keyframe')).toHaveLength(ch === 'video-bytes' ? 3 : 1)
    // The key frame ends it.
    whole(true)
    clock += 300
    whole(false)
    paintAll()
    expect(ofType('request-keyframe')).toHaveLength(ch === 'video-bytes' ? 3 : 1)
    expect(ofType('keyframe-acquired')).toHaveLength(ch === 'video-bytes' ? 1 : 0)
    const types = FakeDecoder.instances[0]!.chunks.map((c) => c.type)
    // 1-2 before the losses, then the frames that completed: gated on
    // video-bytes until the key frame, every one on video-deadline.
    expect(types).toEqual(
      ch === 'video-bytes'
        ? ['key', 'delta', 'key', 'delta']
        : ['key', 'delta', ...Array<string>(20).fill('delta'), 'key', 'delta'],
    )
  })
})

// ── Cell 4: a new viewer and an old agent ─────────────────────────────────────

interface Run {
  posted: Record<string, unknown>[]
  decoded: (string | number)[][]
}

/** Play `stream` on `video-bytes` alone, in order and whole, as an agent of
 *  today does, through a fresh worker. In the moved mode `idleDeadline` moves a
 *  `video-deadline` channel beside it that never carries a message. */
async function play(mode: Mode, stream: StreamFrame[], idleDeadline: boolean): Promise<Run> {
  vi.resetModules()
  FakeDecoder.instances = []
  posted = []
  painted = 0
  clock = 100_000
  const send = await boot({ chunkFraming: true, decodeRecover: true })
  let bytes: (buf: ArrayBuffer) => void
  if (mode === RELAYED) {
    bytes = (buf) => send({ type: 'chunk', bytes: buf, sentAt: performance.timeOrigin + clock })
  } else {
    const ch = new FakeChannel()
    const move: Record<string, unknown> = { type: 'video-channel', channel: ch }
    if (idleDeadline) move.deadline = new FakeChannel()
    send(move)
    ch.open()
    bytes = (buf) => ch.deliver(buf)
  }
  for (const f of stream) {
    clock += f.gapMs
    for (const m of agentChunks(f.seq, f.frame)) bytes(m)
    paintAll()
  }
  return { posted: [...posted], decoded: decoded() }
}

/** Today's worker on an ordered, lossless stream, as a model: every frame from
 *  the first key frame on reaches the decoder in order; the leading deltas wait
 *  at the gate, which asks once a debounce window; nothing is lost or late. */
function expectToday(run: Run, stream: StreamFrame[]): void {
  const first = stream.findIndex((f) => f.key)
  expect(run.decoded).toEqual(stream.slice(first).map((f) => [f.key ? 'key' : 'delta', f.timestampUs]))
  const of = (t: string) => run.posted.filter((m) => m.type === t)
  expect(of('frame-rejected')).toEqual([])
  expect(of('backlog-drop')).toEqual([])
  expect(of('keyframe-acquired')).toEqual([{ type: 'keyframe-acquired', droppedBefore: first }])
  expect(of('awaiting-keyframe')).toEqual(first > 0 ? [{ type: 'awaiting-keyframe', dropped: 1 }] : [])
  let t = 100_000
  let lastAsk = 0
  let asks = 0
  for (const f of stream.slice(0, first)) {
    t += f.gapMs
    if (t - lastAsk >= 250) {
      asks++
      lastAsk = t
    }
  }
  expect(of('request-keyframe')).toHaveLength(asks)
  const stats = of('stats')
  expect(stats.length).toBeGreaterThan(1)
  for (const st of stats) {
    expect(st.chunkGaps).toBe(0)
    expect(st.chunkStragglers).toBe(0)
  }
}

describe('FR-148 P4b-2 cell 4 — an old agent: nothing ever arrives on video-deadline', () => {
  const SEEDS = [0x1234, 0xbeef, 0x51ed, 0x2545f491]

  it.each(SEEDS)('seed %i: the worker decodes and says what it does today, frame for frame', async (seed) => {
    const stream = oldAgentStream(seed)
    // The stream is the shape the seed promises: leading deltas or not, key
    // frames apart, and frames across the chunk edges.
    expect(stream.filter((f) => f.key).length).toBeGreaterThan(1)
    expect(stream.some((f) => f.frame.length > 16 * 1024)).toBe(true)

    for (const mode of MODES) expectToday(await play(mode, stream, true), stream)

    // Moved, the idle channel beside video-bytes changes nothing the worker
    // says or decodes: every message, in order, field for field.
    const alone = await play(MOVED, stream, false)
    const beside = await play(MOVED, stream, true)
    expect(beside.decoded).toEqual(alone.decoded)
    expect(beside.posted).toEqual(alone.posted)
  })
})

// ── FR-148 P4b-5a: the key frames the worker completed whole, by channel ──────

describe.each(MODES)('FR-148 P4b-5a — the key frames completed whole, by channel, %s', (mode) => {
  /** The newest `stats`' count, read at a message 1.2 s on: a delta's lone
   *  chunk on `video-bytes`, which counts nothing. */
  const counted = (s: Session, seq: number) => {
    clock += 1_200
    s.bytes(agentChunks(seq, wireFrame(false, seq * 1_000, 900))[0]!)
    return ofType('stats').at(-1)!.kf
  }

  it('counts a key frame whole on the channel it came on, and a delta on neither', async () => {
    const s = await session(mode)
    const frames: [boolean, number, Channel][] = [
      [true, 40_000, 'video-bytes'],
      [false, 900, 'video-bytes'],
      [true, 40_000, 'video-deadline'],
      [false, 20_000, 'video-deadline'],
      [true, 900, 'video-deadline'],
      [false, 900, 'video-deadline'],
    ]
    frames.forEach(([key, size, ch], i) => {
      for (const m of agentChunks(i + 1, wireFrame(key, (i + 1) * 1_000, size))) on(s, ch)(m)
    })
    expect(counted(s, 7)).toEqual([1, 2])
    // Cumulative from the session's start: the next window keeps them.
    for (const m of agentChunks(8, wireFrame(true, 8_000, 900))) s.bytes(m)
    expect(counted(s, 9)).toEqual([2, 2])
  })

  it('counts no key frame the assembler never completed: evicted while partial, or straggling after', async () => {
    const s = await session(mode)
    const c = (seq: number, key: boolean, size: number) => agentChunks(seq, wireFrame(key, seq * 1_000, size))
    for (const m of c(1, true, 900)) s.bytes(m)
    // Key frame 2: two of its three chunks; then the first chunks of deltas 3
    // and 4, and key frame 5, whole in one chunk, which evicts key frame 2 and
    // takes 3 and 4 as stragglers.
    const two = c(2, true, 40_000)
    s.deadline(two[0]!)
    s.deadline(two[1]!)
    for (let seq = 3; seq <= 4; seq++) s.deadline(c(seq, false, 20_000)[0]!)
    for (const m of c(5, true, 900)) s.deadline(m)
    // Key frame 2's last chunk, and frame 3's, late: stragglers.
    s.deadline(two[2]!)
    s.deadline(c(3, false, 20_000)[1]!)
    expect(counted(s, 6)).toEqual([1, 1])
    const stats = ofType('stats').at(-1)!
    expect(stats.chunkGaps).toBe(1)
    expect(stats.chunkStragglers).toBe(4)
  })
})

describe('FR-148 P4b-5a — the count rides every stats of a framed session, and none of an unframed one', () => {
  it('framed: every stats carries both counts, from [0, 0]', async () => {
    const send = await boot({ chunkFraming: true, decodeRecover: true })
    const chunk = (buf: ArrayBuffer, deadline: boolean) =>
      send({ type: 'chunk', bytes: buf, sentAt: performance.timeOrigin + clock, ...(deadline ? { deadline } : {}) })
    for (let seq = 1; seq <= 12; seq++) {
      clock += 400
      const key = seq % 4 === 1
      for (const m of agentChunks(seq, wireFrame(key, seq * 1_000, 900))) chunk(m, seq > 6)
      paintAll()
    }
    const stats = ofType('stats')
    expect(stats.length).toBeGreaterThan(2)
    for (const st of stats) {
      expect(Array.isArray(st.kf) && (st.kf as unknown[]).length === 2, JSON.stringify(st.kf)).toBe(true)
    }
    // Key frames 1 and 5 on video-bytes, 9 on video-deadline.
    expect(stats.at(-1)!.kf).toEqual([2, 1])
  })

  it('unframed: no stats carries the key', async () => {
    const send = await boot({ chunkFraming: false, decodeRecover: true })
    for (let seq = 1; seq <= 12; seq++) {
      clock += 400
      send({ type: 'chunk', bytes: wireFrame(seq % 4 === 1, seq * 1_000, 900).buffer, sentAt: performance.timeOrigin + clock })
      paintAll()
    }
    const stats = ofType('stats')
    expect(stats.length).toBeGreaterThan(2)
    for (const st of stats) expect(st).not.toHaveProperty('kf')
  })
})
