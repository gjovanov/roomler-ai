// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-135 R2 — how fast this viewer's screen refreshes, read from a decode
 * worker's `requestAnimationFrame`.
 *
 * Until now the viewer never learned its own refresh, so a 60 Hz screen could
 * ask for 144. A decode worker holds an `OffscreenCanvas`, so its rAF ticks with
 * the page. Above-60 sessions are data-channel only, so these workers suffice.
 *
 * It runs in short bursts: an rAF chain that never stops would wake this thread
 * on every refresh, the thread that decodes, for a number that rarely changes.
 *
 * ⚠️ A worker that falls behind reads low, and FR-135's S14 has not yet answered
 * whether its rAF follows the panel under `desynchronized`. So this reading only
 * informs: the agent's cap on it stays off (`ROOMLERD_VIEWER_HZ_CAP`).
 */

/** Intervals in one reading: about a second at 60 Hz, under half at 144. */
export const VIEWER_HZ_INTERVALS = 60
/** A longer gap is a hidden tab or a stalled thread, not a slow screen, so the
 *  reading starts over. */
export const VIEWER_HZ_MAX_GAP_MS = 250
/** Between bursts: the window may move to another screen, and that is rare. */
export const VIEWER_HZ_EVERY_MS = 60_000
/** A reading this close to the last is the same screen. */
export const VIEWER_HZ_SAME = 1

export interface HzMeter {
  /** A tick's timestamp. The reading once it has enough ticks, else null. */
  tick(nowMs: number): number | null
}

export function createHzMeter(): HzMeter {
  let last: number | null = null
  let intervals: number[] = []
  return {
    tick(nowMs: number): number | null {
      const gap = last === null ? null : nowMs - last
      last = nowMs
      if (gap === null) return null
      if (gap > VIEWER_HZ_MAX_GAP_MS || gap <= 0) {
        intervals = []
        return null
      }
      intervals.push(gap)
      if (intervals.length < VIEWER_HZ_INTERVALS) return null
      // A tick the thread missed is a double interval, and rounding on a coarse
      // clock reads 16 and 17 ms in turn. So: the median finds the refresh's own
      // interval, and the mean of the ones near it reads between clock ticks.
      const sorted = [...intervals].sort((a, b) => a - b)
      const median = sorted[Math.floor(sorted.length / 2)]!
      const near = sorted.filter((g) => g < median * 1.5)
      const mean = near.reduce((sum, g) => sum + g, 0) / near.length
      intervals = []
      return Math.round(1000 / mean)
    },
  }
}

export interface HzEnv {
  raf?: (cb: (nowMs: number) => void) => unknown
  later: (run: () => void, ms: number) => unknown
}

function workerEnv(): HzEnv {
  const scope = globalThis as unknown as { requestAnimationFrame?: (cb: (t: number) => void) => unknown }
  return {
    raf: typeof scope.requestAnimationFrame === 'function' ? (cb) => scope.requestAnimationFrame!(cb) : undefined,
    later: (run, ms) => setTimeout(run, ms),
  }
}

/** Measure now and every `VIEWER_HZ_EVERY_MS`, and report a reading that moved.
 *  With no worker rAF (an older browser) it does nothing, and the agent simply
 *  never hears: that is what it hears from an older viewer too. */
export function startViewerHzBursts(report: (hz: number) => void, env: HzEnv = workerEnv()): void {
  const raf = env.raf
  if (!raf) return
  let told: number | null = null
  const burst = () => {
    const meter = createHzMeter()
    const onTick = (nowMs: number) => {
      const hz = meter.tick(nowMs)
      if (hz === null) {
        raf(onTick)
        return
      }
      if (told === null || Math.abs(hz - told) > VIEWER_HZ_SAME) {
        told = hz
        report(hz)
      }
      env.later(burst, VIEWER_HZ_EVERY_MS)
    }
    raf(onTick)
  }
  burst()
}
