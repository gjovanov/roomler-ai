// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #3566 — the page measures each decode worker's clock against its own
 * (`rc-clock-cal.ts`), and the worker reads the page's timestamps through it.
 *
 * The numbers are the Windows dev box's: a worker made 162 s after its page
 * read 7.25 ms behind it. The arithmetic and the page's half are pure and are
 * tested here, and so is the RTP transform worker's bench stamp. The
 * data-channel worker is driven in `rcDcWorker.spec.ts`.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import {
  CAL_MAX_RTT_MS,
  CAL_PING_TIMEOUT_MS,
  CAL_RETRY_MS,
  CAL_ROUND_TRIPS,
  CAL_ROUNDS,
  CAL_SPACING_MS,
  CLOCK_CAL_STORAGE_KEY,
  calSample,
  clockCalPong,
  createClockCalibration,
  fwdHopMs,
  normalizeSkewMs,
  pickCalSample,
  storedClockCal,
  toPageEpochMs,
  workerAgentOffsetUs,
  type ClockCalResult,
} from '@/workers/rc-clock-cal'
import { HopStats, frameAgeMs } from '@/workers/rc-hop-stats'

/** The worker's clock less the page's, measured on the dev box. */
const SKEW = -7.25
/** An epoch stamp on the page's clock, 2026-11-08. */
const T = 1_794_096_000_123.4

describe('#3566 — one round trip', () => {
  it("is the worker's stamp less the midpoint of the page's two", () => {
    // Pinged at T; the worker, 7.25 ms behind, stamps it 0.1 ms later; the
    // answer lands 0.2 ms after the ping.
    const s = calSample(T, T + 0.1 + SKEW, T + 0.2)!
    expect(s.rttMs).toBeCloseTo(0.2, 3)
    expect(s.skewMs).toBeCloseTo(SKEW, 3)
  })

  it('is wrong by at most half the round trip, whichever leg was slow', () => {
    for (const out of [0.05, 2, 3.95]) {
      // A 4 ms round trip whose ping reached the worker `out` ms in.
      const s = calSample(T, T + out + SKEW, T + 4)!
      expect(Math.abs(s.skewMs - SKEW), `out ${out}`).toBeLessThanOrEqual(s.rttMs / 2 + 1e-3)
    }
  })

  it('refuses a stamp that is not a number, and a round trip that ran backwards', () => {
    expect(calSample(Number.NaN, T, T)).toBeNull()
    expect(calSample(T, Number.POSITIVE_INFINITY, T)).toBeNull()
    expect(calSample(T, T, Number.NaN)).toBeNull()
    expect(calSample(T, T, T - 0.1)).toBeNull()
    expect(calSample(T, T + SKEW, T)).toEqual({ skewMs: SKEW, rttMs: 0 })
  })
})

describe('#3566 — the round trip to trust', () => {
  it('is the lowest RTT', () => {
    const best = pickCalSample([
      { skewMs: -7.0, rttMs: 0.6 },
      { skewMs: -7.25, rttMs: 0.1 },
      { skewMs: -7.4, rttMs: 0.3 },
    ])
    expect(best).toEqual({ skewMs: -7.25, rttMs: 0.1 })
  })

  it(`discards the slow ones, and trusts none slower than ${CAL_MAX_RTT_MS} ms`, () => {
    expect(pickCalSample([{ skewMs: 40, rttMs: 80 }, { skewMs: -7.2, rttMs: 0.4 }])).toEqual({ skewMs: -7.2, rttMs: 0.4 })
    expect(pickCalSample([{ skewMs: -5, rttMs: 4 }, { skewMs: -9, rttMs: 1.2 }])).toBeNull()
    expect(pickCalSample([{ skewMs: SKEW, rttMs: CAL_MAX_RTT_MS }])).toEqual({ skewMs: SKEW, rttMs: CAL_MAX_RTT_MS })
    expect(pickCalSample([{ skewMs: SKEW, rttMs: Number.NaN }])).toBeNull()
    expect(pickCalSample([])).toBeNull()
  })
})

describe("#3566 — a worker reads the page's timestamps through the skew", () => {
  it('times a 2 ms page hop as 2 ms, where it read negative and was dropped', () => {
    const sentAt = T // the page's clock
    const arrived = T + 2 + SKEW // the worker's clock, 2 ms later
    const before = new HopStats()
    before.add(arrived - sentAt)
    expect(before.snapshotAndReset().n, 'the hop of before').toBe(0)
    const after = new HopStats()
    after.add(fwdHopMs(arrived, sentAt, SKEW))
    expect(after.snapshotAndReset()).toMatchObject({ n: 1, avgMs: 2 })
  })

  it("reads a frame's age, and its age at arrival, as the page's clock would", () => {
    // The agent's clock runs 5 s ahead of the page's; a frame framed 30 ms
    // before its paint, and 12 ms before its arrival.
    const pageOffsetUs = 5_000_000
    const paintedPageUs = T * 1000
    const wireUs = paintedPageUs + pageOffsetUs - 30_000
    const paintedWorkerUs = paintedPageUs + SKEW * 1000
    const arrivedWorkerUs = paintedWorkerUs - 18_000
    expect(frameAgeMs(wireUs, pageOffsetUs, paintedWorkerUs), 'before: 7.25 ms short').toBeCloseTo(30 + SKEW, 2)
    const offsetUs = workerAgentOffsetUs(pageOffsetUs, SKEW)
    expect(frameAgeMs(wireUs, offsetUs, paintedWorkerUs)).toBeCloseTo(30, 2)
    expect(frameAgeMs(wireUs, offsetUs, arrivedWorkerUs)).toBeCloseTo(12, 2)
  })

  it("puts a worker's paint stamp on the page's clock, where a bench probe's send is", () => {
    const sent = T
    const paintedWorker = T + 41 + SKEW
    expect(paintedWorker - sent, 'before: 7.25 ms short').toBeCloseTo(41 + SKEW, 3)
    expect(toPageEpochMs(paintedWorker, SKEW) - sent).toBeCloseTo(41, 3)
  })

  it('is the arithmetic of before, bit for bit, with no skew', () => {
    const pairs: Array<[number, number]> = [[T + 2.0001, T], [T - 3, T], [T, T], [0.1, 0.3], [T, T + 1e-4]]
    for (const [now, sent] of pairs) {
      expect(Object.is(fwdHopMs(now, sent, normalizeSkewMs(undefined)), now - sent), `${now} ${sent}`).toBe(true)
    }
    for (const off of [5e6, -1.794e15, 0.5, -0, 0]) {
      expect(Object.is(workerAgentOffsetUs(off, 0), off), String(off)).toBe(true)
      expect(Object.is(toPageEpochMs(off, 0), off), String(off)).toBe(true)
    }
  })

  it('keeps a finite skew and nothing else', () => {
    expect(normalizeSkewMs(SKEW)).toBe(SKEW)
    expect(normalizeSkewMs(3_600_000)).toBe(3_600_000)
    for (const v of [Number.NaN, Number.POSITIVE_INFINITY, '7', null, undefined, {}]) {
      expect(normalizeSkewMs(v), String(v)).toBe(0)
    }
  })

  it("answers a ping with the seq it was given and the worker's own stamp", () => {
    expect(clockCalPong(4, T + SKEW)).toEqual({ type: 'clock-cal-pong', seq: 4, tw: T + SKEW })
  })
})

describe('#3566 — the kill switch', () => {
  afterEach(() => {
    localStorage.removeItem(CLOCK_CAL_STORAGE_KEY)
  })

  it('is roomler-rc-clock-cal, on unless it says 0', () => {
    expect(CLOCK_CAL_STORAGE_KEY).toBe('roomler-rc-clock-cal')
    expect(storedClockCal(), 'absent').toBe(true)
    for (const raw of ['1', 'on', 'off', '', 'false']) {
      localStorage.setItem(CLOCK_CAL_STORAGE_KEY, raw)
      expect(storedClockCal(), raw).toBe(true)
    }
    for (const raw of ['0', ' 0 ']) {
      localStorage.setItem(CLOCK_CAL_STORAGE_KEY, raw)
      expect(storedClockCal(), raw).toBe(false)
    }
  })

  it('stays on when storage cannot be read', () => {
    const spy = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('denied')
    })
    expect(storedClockCal()).toBe(true)
    spy.mockRestore()
  })
})

// ── the page's half ─────────────────────────────────────────────────────────

/** A calibration on a clock and timers the test moves, posting to a worker the
 *  test plays. */
function harness(post?: (m: Record<string, unknown>) => void) {
  let now = T
  let nextId = 1
  const timers = new Map<number, { at: number; fn: () => void }>()
  const posts: Record<string, unknown>[] = []
  const results: ClockCalResult[] = []
  const cal = createClockCalibration({
    post: (m) => {
      posts.push(m as unknown as Record<string, unknown>)
      post?.(m as unknown as Record<string, unknown>)
    },
    done: (r) => results.push(r),
    now: () => now,
    setTimer: (fn, ms) => {
      const id = nextId++
      timers.set(id, { at: now + ms, fn })
      return id
    },
    clearTimer: (id) => {
      timers.delete(id as number)
    },
  })
  const pings = () => posts.filter((p) => p.type === 'clock-cal-ping')
  const verdicts = () => posts.filter((p) => p.type === 'clock-cal')
  return {
    cal,
    posts,
    results,
    timers,
    pings,
    verdicts,
    get now() {
      return now
    },
    /** Run every timer due in the next `ms`, in order. */
    advance(ms: number) {
      const until = now + ms
      for (;;) {
        let due: [number, { at: number; fn: () => void }] | null = null
        for (const e of timers) if (e[1].at <= until && (due === null || e[1].at < due[1].at)) due = e
        if (!due) break
        timers.delete(due[0])
        now = due[1].at
        due[1].fn()
      }
      now = until
    },
    /** The worker answers the latest ping: it reached the worker after half of
     *  `rttMs`, and the answer reached the page after all of it. */
    answer(rttMs: number) {
      const ping = pings().at(-1)!
      const t0 = now
      now = t0 + rttMs
      return cal.onWorkerMessage({ type: 'clock-cal-pong', seq: ping.seq, tw: t0 + rttMs / 2 + SKEW })
    },
  }
}

describe("#3566 — the page's half", () => {
  it('pings one round trip at a time, spaced, and gives the worker the fastest one’s skew', () => {
    const h = harness()
    h.cal.start()
    // The first waits for the worker's module to load.
    const rtts = [9, 0.4, 0.3, 0.12, 0.5, 0.2, 0.25, 0.3, 0.6, 0.35]
    for (let i = 0; i < CAL_ROUND_TRIPS; i++) {
      expect(h.pings().map((p) => p.seq), `ping ${i + 1}`).toEqual(Array.from({ length: i + 1 }, (_, k) => k + 1))
      expect(h.answer(rtts[i]!)).toBe(true)
      if (i < CAL_ROUND_TRIPS - 1) {
        h.advance(CAL_SPACING_MS - 1)
        expect(h.pings(), 'not before the spacing').toHaveLength(i + 1)
        h.advance(1)
      }
    }
    expect(h.results).toEqual([
      { ok: true, skewMs: expect.closeTo(SKEW, 3), rttMs: expect.closeTo(0.12, 3), samples: CAL_ROUND_TRIPS, rounds: 1 },
    ])
    expect(h.verdicts()).toEqual([{ type: 'clock-cal', skewMs: (h.results[0] as { skewMs: number }).skewMs }])
    expect(h.timers.size, 'nothing left running').toBe(0)
  })

  it('runs another round when the page was too busy to trust any answer', () => {
    const h = harness()
    h.cal.start()
    for (let i = 0; i < CAL_ROUND_TRIPS; i++) {
      h.answer(5)
      if (i < CAL_ROUND_TRIPS - 1) h.advance(CAL_SPACING_MS)
    }
    expect(h.results, 'no verdict yet').toEqual([])
    expect(h.verdicts()).toEqual([])
    h.advance(CAL_RETRY_MS - 1)
    expect(h.pings()).toHaveLength(CAL_ROUND_TRIPS)
    h.advance(1)
    expect(h.pings(), "round 2's first ping").toHaveLength(CAL_ROUND_TRIPS + 1)
    for (let i = 0; i < CAL_ROUND_TRIPS; i++) {
      h.answer(i === 6 ? 0.3 : 6)
      if (i < CAL_ROUND_TRIPS - 1) h.advance(CAL_SPACING_MS)
    }
    expect(h.results).toEqual([
      { ok: true, skewMs: expect.closeTo(SKEW, 3), rttMs: expect.closeTo(0.3, 3), samples: 2 * CAL_ROUND_TRIPS, rounds: 2 },
    ])
    expect(h.verdicts()).toHaveLength(1)
  })

  it(`gives up after ${CAL_ROUNDS} rounds with none trusted, and the worker keeps its own clock`, () => {
    const h = harness()
    h.cal.start()
    for (let r = 0; r < CAL_ROUNDS; r++) {
      for (let i = 0; i < CAL_ROUND_TRIPS; i++) {
        h.answer(3)
        if (i < CAL_ROUND_TRIPS - 1) h.advance(CAL_SPACING_MS)
      }
      if (r < CAL_ROUNDS - 1) h.advance(CAL_RETRY_MS)
    }
    expect(h.results).toEqual([
      { ok: false, samples: CAL_ROUNDS * CAL_ROUND_TRIPS, rounds: CAL_ROUNDS, bestRttMs: expect.closeTo(3, 3) },
    ])
    expect(h.verdicts(), 'no skew posted').toEqual([])
    h.advance(60_000)
    expect(h.pings()).toHaveLength(CAL_ROUNDS * CAL_ROUND_TRIPS)
    expect(h.timers.size).toBe(0)
  })

  it('ends a round on a ping with no answer, and says so when no round got one', () => {
    const h = harness()
    h.cal.start()
    for (let r = 0; r < CAL_ROUNDS; r++) {
      expect(h.pings(), `round ${r + 1}`).toHaveLength(r + 1)
      h.advance(CAL_PING_TIMEOUT_MS)
      if (r < CAL_ROUNDS - 1) h.advance(CAL_RETRY_MS)
    }
    expect(h.results).toEqual([{ ok: false, samples: 0, rounds: CAL_ROUNDS, bestRttMs: null }])
    expect(h.verdicts()).toEqual([])
  })

  it('drops an answer to a ping it gave up on, and one that is not a stamp', () => {
    const h = harness()
    h.cal.start()
    const late = h.pings()[0]!.seq
    h.advance(CAL_PING_TIMEOUT_MS)
    h.advance(CAL_RETRY_MS)
    expect(h.pings()).toHaveLength(2)
    // Round 1's answer, late: the calibration's own, and dropped.
    expect(h.cal.onWorkerMessage({ type: 'clock-cal-pong', seq: late, tw: h.now + SKEW })).toBe(true)
    // An answer that carries no stamp counts as no sample.
    expect(h.cal.onWorkerMessage({ type: 'clock-cal-pong', seq: 2, tw: 'soon' })).toBe(true)
    for (let i = 1; i < CAL_ROUND_TRIPS; i++) {
      h.advance(CAL_SPACING_MS)
      h.answer(0.2)
    }
    expect(h.results).toEqual([
      { ok: true, skewMs: expect.closeTo(SKEW, 3), rttMs: expect.closeTo(0.2, 3), samples: CAL_ROUND_TRIPS - 1, rounds: 2 },
    ])
  })

  it('stops without a verdict when the path stops', () => {
    const h = harness()
    h.cal.start()
    h.answer(0.2)
    h.cal.stop()
    h.advance(60_000)
    expect(h.pings()).toHaveLength(1)
    expect(h.results).toEqual([])
    expect(h.verdicts()).toEqual([])
    expect(h.timers.size).toBe(0)
    expect(h.cal.onWorkerMessage({ type: 'clock-cal-pong', seq: 2, tw: T }), 'still its own').toBe(true)
  })

  it('starts once, and leaves every other message to the page', () => {
    const h = harness()
    h.cal.start()
    h.cal.start()
    expect(h.pings()).toHaveLength(1)
    for (const m of [{ type: 'stats' }, { type: 'bench-probe', seq: 1, paintedAt: T }, {}, null, undefined]) {
      expect(h.cal.onWorkerMessage(m), JSON.stringify(m)).toBe(false)
    }
  })

  it('stops quietly when the worker cannot be posted to', () => {
    const h = harness(() => {
      throw new Error('worker gone')
    })
    h.cal.start()
    h.advance(60_000)
    expect(h.results).toEqual([])
    expect(h.timers.size).toBe(0)
  })
})

// ── the RTP transform worker ────────────────────────────────────────────────

type Scope = {
  postMessage: (msg: unknown) => void
  onmessage: ((ev: { data: unknown }) => void) | null
  onrtctransform: ((ev: unknown) => Promise<void>) | null
}
const scope = self as unknown as Scope

class FakeDecoder {
  static instances: FakeDecoder[] = []
  static isConfigSupported = async () => ({ supported: true })
  constructor(readonly init: { output: (frame: unknown) => void; error: (err: unknown) => void }) {
    FakeDecoder.instances.push(this)
  }
  configure(): void {}
  decode(): void {}
  close(): void {}
}

/** A decoded frame the bench can clone. */
const frame = () => ({
  displayWidth: 64,
  displayHeight: 64,
  colorSpace: { toJSON: () => ({}) },
  clone: () => ({ close: () => {} }),
  close: () => {},
})

describe('#3566 — the RTP transform worker', () => {
  let originalPost: Scope['postMessage']
  let posted: Record<string, unknown>[] = []
  let clock = 0
  const ofType = (t: string) => posted.filter((m) => m.type === t)
  const flush = async () => {
    for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0))
  }

  beforeEach(() => {
    vi.resetModules()
    FakeDecoder.instances = []
    vi.stubGlobal('VideoDecoder', FakeDecoder)
    vi.stubGlobal('EncodedVideoChunk', class {
      constructor(readonly init: unknown) {}
    })
    clock = 100_000
    vi.spyOn(performance, 'now').mockImplementation(() => clock)
    posted = []
    originalPost = scope.postMessage
    scope.postMessage = (msg) => {
      posted.push(msg as Record<string, unknown>)
    }
    // Every probe read answers probe 7: the paint stamp is what is under test.
    vi.doMock('@/workers/rc-bench-sampler', async (importOriginal) => {
      const mod = await importOriginal<typeof import('@/workers/rc-bench-sampler')>()
      return {
        ...mod,
        createBenchSampler: (deps: Parameters<typeof mod.createBenchSampler>[0]) =>
          mod.createBenchSampler({ ...deps, read: async () => 7 }),
      }
    })
  })

  afterEach(() => {
    vi.doUnmock('@/workers/rc-bench-sampler')
    scope.postMessage = originalPost
    scope.onmessage = null
    scope.onrtctransform = null
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })

  async function boot() {
    await import('@/workers/rc-webcodecs-worker')
    const send = (msg: Record<string, unknown>) => scope.onmessage!({ data: msg })
    send({ type: 'init-canvas', canvas: { width: 0, height: 0, getContext: () => ({ drawImage: () => {} }) }, bench: true })
    let ctl!: ReadableStreamDefaultController<unknown>
    const readable = new ReadableStream<unknown>({
      start(c) {
        ctl = c
      },
    })
    const writable = new WritableStream<unknown>()
    await scope.onrtctransform!({ transformer: { readable, writable, options: { codec: 'h264' } } })
    // One encoded frame through the transform, which also clears its 3 s watchdog.
    ctl.enqueue({ type: 'key', timestamp: 1, data: new ArrayBuffer(8) })
    await flush()
    return send
  }

  it('answers a ping with its own clock, stamped on arrival', async () => {
    const send = await boot()
    send({ type: 'clock-cal-ping', seq: 3 })
    expect(ofType('clock-cal-pong')).toEqual([{ type: 'clock-cal-pong', seq: 3, tw: performance.timeOrigin + clock }])
  })

  it("stamps a bench paint on its own clock until the skew arrives, then on the page's", async () => {
    const send = await boot()
    send({ type: 'bench-arm', seq: 7 })
    FakeDecoder.instances[0]!.init.output(frame())
    await flush()
    send({ type: 'clock-cal', skewMs: SKEW })
    send({ type: 'bench-arm', seq: 7 })
    FakeDecoder.instances[0]!.init.output(frame())
    await flush()
    const probes = ofType('bench-probe')
    expect(probes).toHaveLength(2)
    expect(probes[0]!.paintedAt, 'as before').toBe(performance.timeOrigin + clock)
    expect(probes[1]!.paintedAt).toBeCloseTo(performance.timeOrigin + clock - SKEW, 3)
    // A closed session forgets it.
    send({ type: 'close' })
  })
})
