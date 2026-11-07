// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-135 R0 (§3.6, §6.10) — the decode worker's window of media numbers for
 * the agent: the viewer's half of `agents/roomlerd/src/media_window.rs`.
 *
 * Before this, how long a frame took to decode and paint, and whether the
 * picture froze, stopped at the HUD: `rc-hop-stats.ts` kept means for the
 * diagnostics overlay and nothing reached a session's record. The two
 * data-channel workers now keep this window beside those means, and fold it
 * into their 1 s `stats` message, which the page carries to the agent on
 * `rc:decodestat` (`decodeStatWireMessage`). No new message, no new cadence.
 *
 * | Wire field | Instrument | What it counts |
 * |---|---|---|
 * | `dec_ms`, `paint_ms`, `fwd_ms` | N9 | `decode()` → output, the paint itself, the page → worker hop |
 * | `backlog_ms` | N9 | the decode backlog, sampled once per frame the gate let through |
 * | `arr_gap_ms` | N10, the viewer's half | the interval between whole frames reaching the worker |
 * | `gate_ms` | N12, the viewer's half | the key-frame gate armed after a paint → the next painted frame |
 * | `chunk_gaps` | N9 | the framing layer's gaps, cumulative |
 * | `freezes` | N11 | paint-to-paint gaps over {@link FREEZE_GAP_MS}, cumulative |
 * | `painted` | N16 | frames drawn to the canvas, cumulative |
 * | `decoder_hw` | N9 | whether the browser says a hardware decoder takes this configuration |
 *
 * A `_ms` histogram is twelve log-2 millisecond buckets, `[0,1) [1,2) [2,4) …
 * [512,1024) [1024,∞)`, the agent's own (`media_window::LogHist`), so the
 * agent sums a viewer's windows exactly as it sums its own.
 *
 * ⚠️ Record, never decide. Nothing in the worker or on the agent acts on these:
 * the struggling bit, the backlog drop and `queueMs` are untouched, so FR-63's
 * shadow and the live law read what they read before.
 */

/** How many buckets a histogram has; the agent's `media_window::BUCKETS`. */
export const MEDIA_BUCKETS = 12

/** N11 — two painted frames further apart than this are a freeze. Both
 *  data-channel pumps re-send the last frame every 60 ms on a still screen
 *  (`IDLE_KEEPALIVE`) and the viewer-rate cap stops at 12 fps, so a healthy
 *  session paints closer than this, except across a capture hold: the pumps
 *  send nothing while capture waits (a display off, a session away, a lock).
 *  The agent announces a hold (`rc:media-waiting`, about 2 s in) and the
 *  worker restarts its clock then ({@link ViewerMediaRecorder.onHold}). A hold
 *  shorter than its announcement still counts once, and a host held below
 *  5 fps (`ROOMLERD_FFMPEG_FPS`) counts every paint. */
export const FREEZE_GAP_MS = 200

/** The bucket a sample in ms lands in, as the agent buckets microseconds:
 *  whole milliseconds, then the bit length. Pure for the tests. */
export function logBucket(ms: number): number {
  if (!(ms >= 1)) return 0
  if (ms >= 1024) return MEDIA_BUCKETS - 1
  return 32 - Math.clz32(Math.floor(ms))
}

/** Log-2 millisecond buckets for one window. A non-finite or negative sample
 *  is ignored, as `HopStats` ignores one: a clock hiccup must not count. */
export class LogHist {
  private counts: number[] = new Array<number>(MEDIA_BUCKETS).fill(0)

  add(ms: number): void {
    if (!Number.isFinite(ms) || ms < 0) return
    this.counts[logBucket(ms)] += 1
  }

  /** The window's counts, and a fresh window. */
  take(): number[] {
    const out = this.counts
    this.counts = new Array<number>(MEDIA_BUCKETS).fill(0)
    return out
  }
}

/** One stats window, in the names the page maps onto the wire. */
export interface ViewerMediaWindow {
  dec: number[]
  paint: number[]
  fwd: number[]
  backlog: number[]
  arrGap: number[]
  gate: number[]
  /** Cumulative, the framing layer's own count (`ChunkFramingState.gaps`). */
  chunkGaps: number
  /** Cumulative for the worker's session. */
  freezes: number
  /** Cumulative for the worker's session. */
  painted: number
  /** `null` until the browser has answered, or when it cannot say. */
  decoderHw: boolean | null
}

export type DecodePref = 'prefer-software' | 'prefer-hardware' | 'no-preference'

type ConfigSupport = (config: VideoDecoderConfig) => Promise<{ supported?: boolean } | undefined>

/** N9 — does the browser say a hardware decoder takes this configuration?
 *  `prefer-software` forces software, so false with no question asked.
 *  Otherwise the same config is asked about with `prefer-hardware`: under
 *  `no-preference` the browser takes hardware where it has it. `null` when the
 *  browser has no `isConfigSupported` or the question fails: unknown, which the
 *  wire leaves absent. */
export async function decoderIsHardware(codec: string, pref: DecodePref): Promise<boolean | null> {
  if (pref === 'prefer-software') return false
  const vd = (globalThis as { VideoDecoder?: { isConfigSupported?: ConfigSupport } }).VideoDecoder
  if (typeof vd?.isConfigSupported !== 'function') return null
  try {
    const res = await vd.isConfigSupported({
      codec,
      hardwareAcceleration: 'prefer-hardware',
      optimizeForLatency: true,
    } as VideoDecoderConfig)
    return typeof res?.supported === 'boolean' ? res.supported : null
  } catch {
    return null
  }
}

/** The worker's media window for its session. Times are `performance.now()`
 *  ms, the worker's one clock. */
export interface ViewerMediaRecorder {
  /** N10 — a whole frame reached the worker (its last chunk is in hand). */
  onArrival(nowMs: number): void
  /** N9 — the page posted the chunk `ms` before it arrived here. */
  onForward(ms: number): void
  /** N9 — the decode backlog when a frame passed the gate. */
  onBacklog(ms: number): void
  /** N9 — a frame came out of the decoder `ms` after its `decode()`. */
  onDecoded(ms: number): void
  /** N12 — the key-frame gate closed: a backlog drop, a chunk gap or a new
   *  decoder. Counted only once a frame has been painted (the gate before the
   *  first frame is the time to first frame, not a recovery), and the oldest
   *  arming stands while one waits, since the picture stopped then. */
  onGateArmed(nowMs: number): void
  /** N12 — the key frame with wire timestamp `tsUs` passed the gate. The wait
   *  ends when it, or a frame after it, is painted: the frames the decoder
   *  still held when the gate closed paint first, and are not the recovery. */
  onGateOpened(tsUs: number): void
  /** N9, N11, N12, N16 — the frame with wire timestamp `tsUs` was drawn at
   *  `nowMs`, and its paint took `ms`. */
  onPainted(nowMs: number, ms: number, tsUs: number): void
  /** N9 — ask the browser whether this decoder is hardware. A later ask, or a
   *  reset, drops an earlier ask's answer. */
  probeDecoderHw(codec: string, pref: DecodePref): void
  /** N11, N12 — the agent announced a capture hold (`rc:media-waiting`): the
   *  gap across it is the host waiting, not a stall. The freeze clock and a
   *  gate wait restart at the next paint. */
  onHold(): void
  /** The window for the stats message, and a fresh one; the cumulative counts
   *  carry on. */
  take(chunkGaps: number): ViewerMediaWindow
  /** The worker's session ended. */
  reset(): void
}

export function createViewerMediaRecorder(
  isHardware: (codec: string, pref: DecodePref) => Promise<boolean | null> = decoderIsHardware,
): ViewerMediaRecorder {
  let dec = new LogHist()
  let paint = new LogHist()
  let fwd = new LogHist()
  let backlog = new LogHist()
  let arrGap = new LogHist()
  let gate = new LogHist()
  let lastArrivalMs: number | null = null
  let lastPaintMs: number | null = null
  let gateSinceMs: number | null = null
  let gateKeyTsUs: number | null = null
  let freezes = 0
  let painted = 0
  let decoderHw: boolean | null = null
  let probe = 0
  return {
    onArrival(nowMs) {
      if (lastArrivalMs !== null) arrGap.add(nowMs - lastArrivalMs)
      lastArrivalMs = nowMs
    },
    onForward(ms) {
      fwd.add(ms)
    },
    onBacklog(ms) {
      backlog.add(ms)
    },
    onDecoded(ms) {
      dec.add(ms)
    },
    onGateArmed(nowMs) {
      if (lastPaintMs === null) return
      if (gateSinceMs === null) gateSinceMs = nowMs
      // Closed again before its key frame painted: the next key ends it.
      gateKeyTsUs = null
    },
    onGateOpened(tsUs) {
      if (gateSinceMs !== null) gateKeyTsUs = tsUs
    },
    onPainted(nowMs, ms, tsUs) {
      paint.add(ms)
      painted += 1
      if (lastPaintMs !== null && nowMs - lastPaintMs > FREEZE_GAP_MS) freezes += 1
      lastPaintMs = nowMs
      if (gateSinceMs !== null && gateKeyTsUs !== null && tsUs >= gateKeyTsUs) {
        gate.add(nowMs - gateSinceMs)
        gateSinceMs = null
        gateKeyTsUs = null
      }
    },
    probeDecoderHw(codec, pref) {
      const mine = ++probe
      decoderHw = null
      void isHardware(codec, pref).then(
        (hw) => {
          if (mine === probe) decoderHw = hw
        },
        () => undefined,
      )
    },
    onHold() {
      lastPaintMs = null
      gateSinceMs = null
      gateKeyTsUs = null
    },
    take(chunkGaps) {
      return {
        dec: dec.take(),
        paint: paint.take(),
        fwd: fwd.take(),
        backlog: backlog.take(),
        arrGap: arrGap.take(),
        gate: gate.take(),
        chunkGaps,
        freezes,
        painted,
        decoderHw,
      }
    },
    reset() {
      dec = new LogHist()
      paint = new LogHist()
      fwd = new LogHist()
      backlog = new LogHist()
      arrGap = new LogHist()
      gate = new LogHist()
      lastArrivalMs = null
      lastPaintMs = null
      gateSinceMs = null
      gateKeyTsUs = null
      freezes = 0
      painted = 0
      decoderHw = null
      probe += 1
    },
  }
}
