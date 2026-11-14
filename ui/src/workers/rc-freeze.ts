// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-135 N11 (#3875) — when a gap between two painted frames is a freeze.
 *
 * FR-135 §3.5's freeze is a paint gap over 200 ms on moving content. Until #3875
 * the worker counted every paint gap over 200 ms, so the gap where motion
 * stopped on the host counted too, and so did every gap of a host that frames
 * sparsely: R0's week read 41.3 per 10 min direct against a target of 1. The
 * host's own pauses are in the frames already. A frame's header carries
 * `timestamp_us`, the agent's process clock when it framed the frame
 * (`rc-dc-worker.ts:24-30`), so the worker can see how long the host itself
 * framed nothing.
 *
 * **The rule.** Take two frames painted one after the other, at `p0` and `p1`
 * on the worker's clock, framed at `h0` and `h1` on the agent's. Every header
 * the worker saw framed between them counts as framed, painted or not: a
 * delta dropped at the key-frame gate or by the backlog shed was framed all
 * the same. The host's pause is the longest stretch between neighbours among
 * `h0`, those headers and `h1`. The gap is a freeze when
 *
 *     (p1 − p0) − the host's longest pause  >  FREEZE_GAP_MS
 *
 * that is, when the viewer held the picture more than 200 ms longer than the
 * host's own framing explains. Both sides are intervals, each on its own
 * clock, so no clock offset enters it (the agent's and the worker's clocks are
 * never compared, `docs/remote-control.md` §11.14.3).
 *
 * | Case | What the rule says |
 * |---|---|
 * | motion stops, a sparse frame (a caret), a host at 4 fps | the host's pause is the gap: no freeze |
 * | the path or the decoder holds frames the host framed steadily | a freeze, once, at the first frame painted after it |
 * | the gate waits for a key frame while deltas arrive and are dropped | a freeze: the dropped deltas were framed |
 * | the host stalls (capture, an encoder open) and the viewer adds under 200 ms | no freeze: the agent counts its own (`pump_stalls`, `iter_ms_max`, `open_ms_max`) |
 * | a frame the agent shed before sending it, or its source gate kept from being made | a host pause, the agent's to count (`frames_skipped_backpressure`) |
 * | the first paint; the paint after a capture hold (`hold`); a timestamp that runs backwards | not judged; the clock starts at that paint |
 * | a keepalive re-encode | a frame like any other: the header cannot tell one apart |
 *
 * ⚠️ Keepalives count. On a still screen the data-channel pumps re-send the
 * last frame every 60 ms (`IDLE_KEEPALIVE`, `agents/roomlerd/src/peer.rs`), and
 * a stall between two of them is counted, though no content changed and nobody
 * saw it. No signal the viewer has tells a keepalive apart: payload size fails
 * on `hevc_qsv`, whose keepalives run about 64 KB, the size of a motion delta,
 * and cadence fails on real frames at the viewer-rate cap or a 15 fps capture,
 * 66 to 83 ms apart. A read of FR-135 §3.5's row takes moving windows (§2.1).
 *
 * ⚠️ A hidden tab counts like a visible one. The worker paints from the
 * decoder's output, not from `requestAnimationFrame`, so its gaps there are
 * the path's; and the read's denominator, the agent's moving windows, cannot
 * leave hidden time out, so a count that did would read low.
 *
 * Record, never decide: nothing acts on the count.
 */

/** N11 — a paint gap is a freeze when it exceeds the host's own longest pause
 *  between the two painted frames by more than this. */
export const FREEZE_GAP_MS = 200

/** The most headers kept, one each, between paints. Past it the oldest half
 *  folds into one span that keeps its longest pause exactly, so an outage with
 *  thousands of frames framed and none painted costs a bounded array. */
export const MAX_FRAMED = 2048

/** N11's verdict, pure: the viewer held the picture `paintGapMs`, the host's
 *  own framing paused `hostPauseMs` at most in that time. */
export function isFreeze(paintGapMs: number, hostPauseMs: number): boolean {
  return paintGapMs - hostPauseMs > FREEZE_GAP_MS
}

export interface FreezeCounter {
  /** The header of a frame the host framed at `tsUs` (its wire timestamp)
   *  reached the worker, whatever then became of the frame: decoded, dropped
   *  at the key-frame gate or by the backlog shed. */
  framed(tsUs: number): void
  /** The frame with wire timestamp `tsUs` was drawn at `nowMs`, the worker's
   *  `performance.now()`. True when the gap it ends is a freeze. */
  painted(nowMs: number, tsUs: number): boolean
  /** The agent announced a capture hold (`rc:media-waiting`): the gap to the
   *  next paint is not judged. */
  hold(): void
  /** Freezes since the last reset. */
  readonly count: number
  /** A new session: forget every frame. */
  reset(): void
}

/** Folded headers: framed from `first` to `last`, no two neighbours more than
 *  `inner` µs apart. */
interface Span {
  first: number
  last: number
  inner: number
}

export function createFreezeCounter(): FreezeCounter {
  let lastPaintMs: number | null = null
  let lastPaintUs: number | null = null
  /** Headers framed after the picture, in order, past the folded `span`. */
  let framed: number[] = []
  let span: Span | null = null
  let count = 0

  /** Fold the oldest half of `framed` into `span`. */
  function fold(): void {
    const k = framed.length >> 1
    const first = span ? span.first : framed[0]!
    let prev = span ? span.last : framed[0]!
    let inner = span ? span.inner : 0
    for (let i = 0; i < k; i += 1) {
      const ts = framed[i]!
      inner = Math.max(inner, ts - prev)
      prev = ts
    }
    span = { first, last: prev, inner }
    framed = framed.slice(k)
  }

  /** The host's longest pause, µs, from the picture framed at `fromUs` to the
   *  frame framed at `toUs`. A paint inside the folded span reads the span's
   *  longest pause whole: never shorter than the truth, so never more freezes. */
  function longestPauseUs(fromUs: number, toUs: number): number {
    let prev = fromUs
    let longest = 0
    if (span && span.first < toUs) {
      longest = Math.max(longest, span.first - prev, span.inner)
      if (span.last >= toUs) return longest
      prev = Math.max(prev, span.last)
    }
    for (const ts of framed) {
      if (ts >= toUs) break
      if (ts <= prev) continue
      longest = Math.max(longest, ts - prev)
      prev = ts
    }
    return Math.max(longest, toUs - prev)
  }

  /** Forget the headers framed up to the picture at `tsUs`. */
  function passed(tsUs: number): void {
    if (span && span.last <= tsUs) span = null
    // Usually the painted frame alone: shifted in place, nothing allocated.
    while (framed.length > 0 && framed[0]! <= tsUs) framed.shift()
  }

  function forgetFrames(): void {
    framed = []
    span = null
  }

  return {
    framed(tsUs) {
      if (!Number.isFinite(tsUs) || tsUs < 0) return
      // Behind the picture: it says nothing about the gaps to come.
      if (lastPaintUs !== null && tsUs <= lastPaintUs) return
      // Inside or behind the folded span: left out, which can only lengthen a
      // pause, and so only ever count less.
      if (span && tsUs <= span.last) return
      const n = framed.length
      if (n === 0 || tsUs > framed[n - 1]!) {
        framed.push(tsUs)
      } else {
        // Behind a later header. The worker's assembler sees whole frames in
        // order, so this does not happen there, but the host did frame it.
        let i = n
        while (i > 0 && framed[i - 1]! > tsUs) i -= 1
        if (i > 0 && framed[i - 1] === tsUs) return
        framed.splice(i, 0, tsUs)
      }
      if (framed.length > MAX_FRAMED) fold()
    },
    painted(nowMs, tsUs) {
      if (!Number.isFinite(nowMs) || !Number.isFinite(tsUs)) return false
      let froze = false
      if (lastPaintUs !== null && tsUs < lastPaintUs) {
        // The agent's clock ran backwards: another process behind this
        // worker. Its old headers mean nothing against the new clock.
        forgetFrames()
      } else if (lastPaintMs !== null && lastPaintUs !== null && tsUs > lastPaintUs) {
        const gapMs = nowMs - lastPaintMs
        froze = gapMs > FREEZE_GAP_MS && isFreeze(gapMs, longestPauseUs(lastPaintUs, tsUs) / 1000)
      }
      if (froze) count += 1
      passed(tsUs)
      lastPaintMs = nowMs
      lastPaintUs = tsUs
      return froze
    },
    hold() {
      lastPaintMs = null
    },
    get count() {
      return count
    },
    reset() {
      lastPaintMs = null
      lastPaintUs = null
      forgetFrames()
      count = 0
    },
  }
}
