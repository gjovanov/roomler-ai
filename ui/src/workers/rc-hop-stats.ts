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

// ── P6 (Parsec-class plan) — flow-control knobs ─────────────────────────────
// The rc.188 viewer-rate loop had two hardwired constants sized for a janky
// (pre-P1) viewer: the workers' backlog-drop threshold (queue > 4) and the
// composable's INSTANTANEOUS per-window struggling rule (any backlog drop OR
// queue > 2 inside a single 1 s window). P1's field read showed a healthy
// viewer parks at queue 0 — so one bad window (e.g. a large IDR landing while
// the tab briefly lost the GPU) tripped the struggling bit, the agent clamped
// its send-fps, and the lazy recovery took ~20 s to climb back. Both knobs are
// now configurable (init-canvas / localStorage), and the struggling bit needs
// a SUSTAINED run of bad windows before it fires.

/** Worker decode-queue depth above which non-key frames are dropped and an
 *  IDR resync is requested. */
export const DEFAULT_MAX_DECODE_QUEUE = 4
/** Queue depth that makes a 1 s stats window count as "bad" for the
 *  struggling rule (strictly-greater compare). */
export const DEFAULT_STRUGGLE_QUEUE = 2
/** Consecutive bad windows before the struggling bit is reported to the
 *  agent. 1 = the legacy instantaneous rule. */
export const DEFAULT_STRUGGLE_WINDOWS = 2

/** Parse an integer knob (localStorage string or init-canvas number) with a
 *  clamp. Anything non-numeric (null, '', 'banana') → the default; fractions
 *  truncate toward zero BEFORE clamping. */
export function normalizeIntKnob(
  raw: unknown,
  def: number,
  min: number,
  max: number,
): number {
  const n =
    typeof raw === 'number'
      ? raw
      : typeof raw === 'string'
        ? Number.parseInt(raw, 10)
        : Number.NaN
  if (!Number.isFinite(n)) return def
  return Math.min(max, Math.max(min, Math.trunc(n)))
}

/** Sustained-window struggle fold. Call `observe(bad)` once per 1 s stats
 *  window; it returns true only after `windows` CONSECUTIVE bad windows (and
 *  keeps returning true while the bad run continues). A single clean window
 *  resets the streak — recovery is immediate, assertion is lazy. */
export class StruggleWindow {
  private streak = 0
  private readonly windows: number

  constructor(windows: number) {
    this.windows = Math.max(1, Math.trunc(Number.isFinite(windows) ? windows : 1))
  }

  observe(bad: boolean): boolean {
    this.streak = bad ? this.streak + 1 : 0
    return this.streak >= this.windows
  }

  reset(): void {
    this.streak = 0
  }
}
