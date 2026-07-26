/**
 * P1 (Parsec-class plan) — shared pure helpers for the decode workers'
 * per-hop instrumentation. The NEO16 field ceiling (~25-35 fps on an RTX
 * 5090 viewer) could be decode-bound, paint-bound, or main-thread-bound —
 * indistinguishable without per-hop numbers. Each worker accumulates
 * per-window hop timings with `HopStats` and folds the snapshots into its
 * existing 1 s `stats` message (no new message cadence), so the diagnosis
 * costs nothing at steady state.
 *
 * Hops measured:
 *  - `fwd`    — main-thread DC `onmessage` → worker `chunk` arrival
 *               (epoch-absolute clocks via `epochNowMs`, valid across
 *               contexts because both sides use timeOrigin + now()).
 *  - `decode` — `decoder.decode(chunk)` submission → `output` callback
 *               (queue + decoder latency; indicts the decoder itself).
 *  - `paint`  — the `paintFrame` body (canvas blit cost).
 *
 * Also home to the 2D-context A/B options: `alpha:false` enables the
 * opaque compositor fast path; `desynchronized:true` requests the
 * low-latency canvas swap chain (may be a no-op for placeholder-composited
 * OffscreenCanvas — which is exactly why it ships as an A/B, not an
 * assumed win). localStorage `roomler-rc-ctx-mode` drives it.
 */

/** One window's aggregate for a hop. */
export type HopWindow = { avgMs: number; maxMs: number; n: number }

/** Round to 0.1 ms — enough resolution for hop diagnosis, keeps the
 *  stats payload compact. */
export function round1(v: number): number {
  return Math.round(v * 10) / 10
}

/** Rolling per-window accumulator: `add()` samples, `snapshotAndReset()`
 *  once per stats window. Non-finite / negative samples are ignored (a
 *  clock hiccup must not poison the window). */
export class HopStats {
  private sum = 0
  private max = 0
  private n = 0

  add(ms: number): void {
    if (!Number.isFinite(ms) || ms < 0) return
    this.sum += ms
    if (ms > this.max) this.max = ms
    this.n++
  }

  snapshotAndReset(): HopWindow {
    const w: HopWindow = {
      avgMs: this.n > 0 ? round1(this.sum / this.n) : 0,
      maxMs: round1(this.max),
      n: this.n,
    }
    this.sum = 0
    this.max = 0
    this.n = 0
    return w
  }
}

/** Epoch-absolute milliseconds, comparable across window/worker contexts
 *  (each context's `performance.now()` is relative to its own
 *  `timeOrigin`; adding them re-bases onto the shared epoch). */
export function epochNowMs(): number {
  return performance.timeOrigin + performance.now()
}

/** 2D-context A/B mode. `legacy` = today's optionless `getContext('2d')`. */
export type CtxMode = 'legacy' | 'opaque' | 'opaque-desync'

export function normalizeCtxMode(v: unknown): CtxMode {
  return v === 'legacy' || v === 'opaque' || v === 'opaque-desync'
    ? v
    : 'opaque-desync'
}

/** Context settings for a mode. `undefined` = call getContext with no
 *  options (bit-identical to the pre-P1 behaviour). */
export function ctxOptionsFor(
  mode: CtxMode,
): CanvasRenderingContext2DSettings | undefined {
  if (mode === 'opaque') return { alpha: false }
  if (mode === 'opaque-desync') return { alpha: false, desynchronized: true }
  return undefined
}
