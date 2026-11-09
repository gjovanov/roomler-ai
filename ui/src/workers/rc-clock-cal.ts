// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #3566 — the page measures each decode worker's clock against its own, once,
 * and the worker reads the page's timestamps through what it measured.
 *
 * Three of the viewer's numbers subtract a time the page took from a time a
 * decode worker took:
 *
 * | Number | The page stamps | The worker stamps |
 * |---|---|---|
 * | `fwd_ms` (FR-135 N9) | a relayed chunk's `sentAt` | the chunk's arrival |
 * | the age, and the age at arrival (FR-1 P7, FR-70 M0) | the agent's clock against its own (`rc:clock`) | the paint, the arrival |
 * | L4 (FR-16) | a bench probe's send | the paint that carried it |
 *
 * Both sides read `performance.timeOrigin + performance.now()`, and that is not
 * one clock. Each context fixes its `timeOrigin` from the wall clock when it
 * starts, and `now()` then runs on the monotonic clock. On a host whose wall
 * clock is slewed against the monotonic clock, a worker made after its page
 * reads a constant apart from it: the slew between the two starts. In Chrome on
 * the Windows dev box (2026-11-08), a worker made 162 s after its page read
 * 7.25 ms behind it, about 45 ppm; at that rate a page open for an hour puts it
 * near 160 ms. It stays fixed for the worker's life, because both `now()`s run
 * on one monotonic clock, so one measurement when the worker starts is enough.
 *
 * The measurement is NTP's. The page stamps a ping (`t0`), the worker stamps it
 * on arrival (`tw`) and answers, and the page stamps the answer (`t1`). The
 * worker's skew, its clock less the page's, is `tw − (t0 + t1) / 2`, wrong by
 * at most half the round trip. So the round trip with the lowest RTT is the one
 * kept, and none slower than {@link CAL_MAX_RTT_MS} is trusted. The pings go one
 * at a time, {@link CAL_SPACING_MS} apart, so a page busy at the session's start
 * slows some answers and not all of them. A round with no trusted answer runs
 * again {@link CAL_RETRY_MS} later, up to {@link CAL_ROUNDS} rounds, and then the
 * page says on the console that it measured nothing.
 *
 * Nothing waits for it. Until the skew arrives, and for the whole session when
 * none does (no trusted answer, or `roomler-rc-clock-cal` set to `0`), the
 * worker holds a skew of 0, and each reading through it is the arithmetic of
 * before #3566, bit for bit.
 *
 * ⚠️ The skew is only as fine as the browser's clock. Outside a cross-origin
 * isolated page Chrome coarsens `now()` to 100 µs ({@link CLOCK_GRAIN_MS}),
 * which adds about that to the half round trip. That error is constant for the
 * worker's life, and a relayed hop is about its size, so a real `fwd` hop can
 * read a hair below zero. One below zero by no more than the error
 * ({@link calBandMs}) counts as 0, and one further below is dropped as a fault
 * (#3596, {@link fwdSampleMs}). A browser that coarsens to a millisecond or more
 * measures the skew to its grain, as it times every hop to its grain.
 */
import { epochNowMs } from './rc-hop-stats'

/** The per-browser kill switch: `localStorage.setItem('roomler-rc-clock-cal',
 *  '0')`, then reconnect, and the page measures no worker's clock, so each
 *  decode worker reads the page's timestamps on its own clock, as before #3566.
 *  Anything else, no value, or storage that cannot be read leaves it on. Read
 *  once per connect, like every `roomler-rc-*` knob. */
export const CLOCK_CAL_STORAGE_KEY = 'roomler-rc-clock-cal'

export function storedClockCal(): boolean {
  try {
    const raw = globalThis.localStorage?.getItem(CLOCK_CAL_STORAGE_KEY)
    return (raw ?? '').trim() !== '0'
  } catch {
    return true
  }
}

/** Round trips in a round. The first one also waits for the worker's module to
 *  load, since a worker reads no message before its script has run, so it is
 *  seldom the one kept. */
export const CAL_ROUND_TRIPS = 10
/** The wait after an answer before the next ping: ten of them span about a
 *  fifth of a second of the session's start, not one busy moment of it. */
export const CAL_SPACING_MS = 20
/** A ping with no answer by then ends its round. */
export const CAL_PING_TIMEOUT_MS = 1_000
/** The slowest round trip whose skew is trusted: it is wrong by at most half of
 *  this. A postMessage round trip to an idle worker takes 0.1 to 0.2 ms. */
export const CAL_MAX_RTT_MS = 1
/** Rounds before the page gives up and the worker keeps its own clock. */
export const CAL_ROUNDS = 3
/** The wait between rounds. */
export const CAL_RETRY_MS = 2_000

/** Page → worker: answer at once. */
export interface ClockCalPing {
  type: 'clock-cal-ping'
  seq: number
}

/** Worker → page: `tw` is the worker's `epochNowMs()` when the ping arrived. */
export interface ClockCalPong {
  type: 'clock-cal-pong'
  seq: number
  tw: number
}

/** Page → worker: the worker's clock less the page's, in ms, and the round trip
 *  it was measured on, whose half bounds its error (#3596). */
export interface ClockCalMessage {
  type: 'clock-cal'
  skewMs: number
  rttMs: number
}

/** The worker's answer to ping `seq`, stamped `tw` on the ping's arrival. */
export function clockCalPong(seq: number, tw: number): ClockCalPong {
  return { type: 'clock-cal-pong', seq, tw }
}

/** One round trip. `skewMs` is the worker's clock less the page's. */
export interface CalSample {
  skewMs: number
  rttMs: number
}

/** A round trip from its three stamps: the page's `t0` at the ping, the
 *  worker's `tw` at its arrival, the page's `t1` at the answer. Null for a stamp
 *  that is not a finite number, or a round trip that ran backwards. */
export function calSample(t0: number, tw: number, t1: number): CalSample | null {
  if (!Number.isFinite(t0) || !Number.isFinite(tw) || !Number.isFinite(t1)) return null
  const rttMs = t1 - t0
  if (rttMs < 0) return null
  // `tw − t0` first: two epoch stamps a few ms apart subtract exactly.
  return { skewMs: tw - t0 - rttMs / 2, rttMs }
}

/** The round trip to trust: the lowest RTT among those no slower than
 *  `maxRttMs`. Null when none is, and the worker reads as before. */
export function pickCalSample(
  samples: readonly CalSample[],
  maxRttMs: number = CAL_MAX_RTT_MS,
): CalSample | null {
  let best: CalSample | null = null
  for (const s of samples) {
    if (!(s.rttMs <= maxRttMs)) continue
    if (best === null || s.rttMs < best.rttMs) best = s
  }
  return best
}

/** A skew as a worker keeps it: a finite number, else 0, the reading of before
 *  #3566. */
export function normalizeSkewMs(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) ? v : 0
}

/** FR-135 N9 — the page → worker hop of a relayed chunk, on one clock: the
 *  worker's clock when it arrived (`nowMs`), less the page's `sentAt`, less the
 *  worker's skew. With a skew of 0 it is `nowMs − sentAtMs`, bit for bit. */
export function fwdHopMs(nowMs: number, sentAtMs: number, skewMs: number): number {
  return nowMs - sentAtMs - skewMs
}

/** Chrome's grain on `now()` outside a cross-origin isolated page: it coarsens
 *  every stamp to 100 µs. */
export const CLOCK_GRAIN_MS = 0.1

/** #3596 — how far below zero a `fwd` reading of a real hop can fall, from the
 *  round trip the skew was measured on: half of it plus a grain is the skew's
 *  own error, and the hop's two stamps add a grain more. That holds on a clock
 *  that truncates each stamp to its grain. Chrome rounds a stamp up or down at a
 *  random point in its grain, so a hop can, rarely, read further below, and is
 *  dropped as before. 0 for anything that is not a round trip: no band. */
export function calBandMs(rttMs: unknown): number {
  if (typeof rttMs !== 'number' || !Number.isFinite(rttMs) || rttMs < 0) return 0
  return rttMs / 2 + 2 * CLOCK_GRAIN_MS
}

/** #3596 — a `fwd` reading as the accumulators take it: at or above zero as it
 *  is, below zero by no more than `bandMs` as 0 (a real hop inside the clock's
 *  error), and anything else null, a fault the caller drops. With a band of 0
 *  it keeps exactly the readings `HopStats.add` and `LogHist.add` keep. */
export function fwdSampleMs(rawMs: number, bandMs: number): number | null {
  if (!Number.isFinite(rawMs)) return null
  if (rawMs >= 0) return rawMs
  return rawMs >= -bandMs ? 0 : null
}

/** FR-1 P7, FR-70 M0 — the agent's clock against the worker's, µs, which
 *  `frameAgeMs` takes in the worker, from the offset the page's `rc:clock`
 *  probe measured: the agent's clock against the page's. With a skew of 0 it is
 *  `pageOffsetUs`, bit for bit. */
export function workerAgentOffsetUs(pageOffsetUs: number, skewMs: number): number {
  return pageOffsetUs - skewMs * 1000
}

/** FR-16 L4 — a time on the worker's clock, on the page's: a worker's paint
 *  stamp as the page compares it with a bench probe's send. With a skew of 0 it
 *  is `workerMs`, bit for bit. */
export function toPageEpochMs(workerMs: number, skewMs: number): number {
  return workerMs - skewMs
}

/** How a calibration ended: the skew it gave the worker, or why it gave none. */
export type ClockCalResult =
  | { ok: true; skewMs: number; rttMs: number; samples: number; rounds: number }
  | { ok: false; samples: number; rounds: number; bestRttMs: number | null }

export interface ClockCalibration {
  /** Send the first ping. Once: a second call does nothing. */
  start(): void
  /** A message from the worker. True when it was the calibration's own (a
   *  `clock-cal-pong`), which the caller then drops. Call it before anything
   *  else the message does: the answer is stamped here. */
  onWorkerMessage(msg: { type?: unknown; seq?: unknown; tw?: unknown } | null | undefined): boolean
  /** The worker's path stopped: no more pings, and no verdict. */
  stop(): void
}

export interface ClockCalDeps {
  /** Post to the worker: each ping, then the skew. */
  post: (msg: ClockCalPing | ClockCalMessage) => void
  /** Called once, unless `stop` comes first: how it ended. */
  done: (result: ClockCalResult) => void
  /** The page's clock, epoch ms. */
  now?: () => number
  setTimer?: (fn: () => void, ms: number) => unknown
  clearTimer?: (timer: unknown) => void
}

/** The page's half: ping the worker one round trip at a time, keep the samples,
 *  and post the skew of the lowest trusted RTT (`clock-cal`). Pure: the clock,
 *  the timers and the worker are given. */
export function createClockCalibration(deps: ClockCalDeps): ClockCalibration {
  const now = deps.now ?? epochNowMs
  const setTimer = deps.setTimer ?? ((fn: () => void, ms: number) => setTimeout(fn, ms))
  const clearTimer = deps.clearTimer ?? ((t: unknown) => clearTimeout(t as ReturnType<typeof setTimeout>))
  const samples: CalSample[] = []
  let rounds = 0
  let inRound = 0
  let seq = 0
  let pending: { seq: number; t0: number } | null = null
  let timer: unknown = null
  let over = false

  function arm(fn: () => void, ms: number): void {
    if (timer !== null) clearTimer(timer)
    timer = setTimer(() => {
      timer = null
      fn()
    }, ms)
  }

  function halt(): void {
    over = true
    pending = null
    if (timer !== null) clearTimer(timer)
    timer = null
  }

  function finish(result: ClockCalResult): void {
    halt()
    if (result.ok) {
      try {
        deps.post({ type: 'clock-cal', skewMs: result.skewMs, rttMs: result.rttMs })
      } catch {
        return // the worker is gone, and its path with it
      }
    }
    deps.done(result)
  }

  function endRound(): void {
    pending = null
    const best = pickCalSample(samples)
    if (best) {
      finish({ ok: true, skewMs: best.skewMs, rttMs: best.rttMs, samples: samples.length, rounds })
    } else if (rounds < CAL_ROUNDS) {
      arm(startRound, CAL_RETRY_MS)
    } else {
      let bestRttMs: number | null = null
      for (const s of samples) if (bestRttMs === null || s.rttMs < bestRttMs) bestRttMs = s.rttMs
      finish({ ok: false, samples: samples.length, rounds, bestRttMs })
    }
  }

  function ping(): void {
    if (over) return
    seq += 1
    inRound += 1
    pending = { seq, t0: now() }
    try {
      deps.post({ type: 'clock-cal-ping', seq })
    } catch {
      halt() // the worker is gone, and its path with it
      return
    }
    arm(endRound, CAL_PING_TIMEOUT_MS)
  }

  function startRound(): void {
    if (over) return
    rounds += 1
    inRound = 0
    ping()
  }

  return {
    start() {
      if (over || rounds > 0) return
      startRound()
    },
    onWorkerMessage(msg) {
      if (msg?.type !== 'clock-cal-pong') return false
      const t1 = now()
      if (over || pending === null || msg.seq !== pending.seq) return true
      const s = calSample(pending.t0, typeof msg.tw === 'number' ? msg.tw : Number.NaN, t1)
      pending = null
      if (s) samples.push(s)
      if (inRound >= CAL_ROUND_TRIPS) endRound()
      else arm(ping, CAL_SPACING_MS)
      return true
    },
    stop: halt,
  }
}
