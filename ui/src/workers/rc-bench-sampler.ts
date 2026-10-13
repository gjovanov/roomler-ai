// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-16 L4 — the decode worker's half of the bench probe.
 *
 * The main thread sends a probe on the input channel, then arms this sampler
 * with its number (`bench-arm`). While a probe is outstanding, the worker
 * clones each decoded frame before painting it, stamps the paint, and reads
 * the bottom-right marker off the clone. The first painted frame carrying an
 * armed probe answers it with `bench-probe {seq, paintedAt}`, and the main
 * thread subtracts the send time on the same epoch clock.
 *
 * ⚠️ Cost is paid only while a probe is outstanding. A GPU-backed frame
 * reads back whole (about 1–2 ms at 1080p) however small the rect, so
 * sampling every frame of a session would tax the very path being measured.
 * A probe is answered within a few frames; an unanswered one expires (the
 * input channel is unreliable by design, so probes do get lost).
 *
 * ⚠️ Reads run strictly in paint order, one at a time. `copyTo` is async,
 * and if a later frame's read could finish first, a probe would be
 * attributed to a frame painted after the one that first carried it.
 *
 * Never enabled outside bench mode: `setEnabled` comes from the worker's
 * init message, which the composable sets only when the knob is on AND the
 * agent advertised `bench`.
 */
import { decodeLumaAt, lumaFromRgba, probeRegion } from './rc-bench-marker'

export interface BenchProbeMessage {
  type: 'bench-probe'
  seq: number
  /** Epoch ms (`epochNowMs`) right after the frame was handed to the canvas. */
  paintedAt: number
}

/** An armed probe that has not come back by then is dropped. */
export const BENCH_ARM_TTL_MS = 2_000
/** Frames held for a read at once. Two covers a read that spans a paint. */
const MAX_HELD = 2

export interface BenchSampler {
  setEnabled(on: boolean): void
  /** The main thread sent probe `seq`. */
  arm(seq: number): void
  /** Before paint: a clone of `frame` when a probe is outstanding, else
   *  null. The caller paints `frame` as usual and hands the clone back. */
  take(frame: VideoFrame): VideoFrame | null
  /** After paint: read the marker off the clone (which this closes) and
   *  answer the armed probe it carries, if any. */
  settle(clone: VideoFrame, paintedAt: number): void
  /** For tests: outstanding probes. */
  armedCount(): number
}

export interface BenchSamplerDeps {
  post: (msg: BenchProbeMessage) => void
  now: () => number
  /** Injected for tests; defaults to reading the frame. */
  read?: (frame: VideoFrame) => Promise<number | null>
}

export function createBenchSampler(deps: BenchSamplerDeps): BenchSampler {
  const read = deps.read ?? readProbe
  let enabled = false
  const armed = new Map<number, number>()
  let held = 0
  let chain: Promise<void> = Promise.resolve()

  function prune(nowMs: number) {
    for (const [seq, at] of armed) {
      if (nowMs - at > BENCH_ARM_TTL_MS) armed.delete(seq)
    }
  }

  return {
    setEnabled(on) {
      enabled = on
      if (!on) armed.clear()
    },
    arm(seq) {
      if (!enabled) return
      armed.set(seq & 0x0fff, deps.now())
    },
    take(frame) {
      if (!enabled || armed.size === 0) return null
      prune(deps.now())
      if (armed.size === 0 || held >= MAX_HELD) return null
      try {
        const clone = frame.clone()
        held++
        return clone
      } catch {
        return null
      }
    },
    settle(clone, paintedAt) {
      chain = chain
        .then(() => read(clone))
        .then(
          (seq) => {
            if (seq !== null && armed.delete(seq)) deps.post({ type: 'bench-probe', seq, paintedAt })
          },
          () => undefined,
        )
        .finally(() => {
          held--
          try {
            clone.close()
          } catch {
            /* already closed */
          }
        })
    },
    armedCount() {
      return armed.size
    },
  }
}

const YUV_FORMATS = new Set(['I420', 'I420A', 'I422', 'I444', 'NV12'])
const RGB_ORDER: Record<string, 'rgba' | 'bgra'> = {
  RGBA: 'rgba',
  RGBX: 'rgba',
  BGRA: 'bgra',
  BGRX: 'bgra',
}

/** Read the probe marker from the bottom-right of a frame's visible area.
 *  `null` for a format with no 8-bit luma to read (10-bit, unknown). */
export async function readProbe(frame: VideoFrame): Promise<number | null> {
  const vr = frame.visibleRect
  const fmt = frame.format
  if (!vr || !fmt) return null
  const rect = probeRegion(vr.width, vr.height, vr.x, vr.y)
  if (!rect) return null
  const yuv = YUV_FORMATS.has(fmt)
  const rgb = RGB_ORDER[fmt]
  if (!yuv && !rgb) return null
  // A 4:2:0 copy refuses an odd rect; the encoder never makes one.
  if (yuv && fmt !== 'I444' && (rect.width % 2 || rect.height % 2)) return null
  const buf = new Uint8Array(frame.allocationSize({ rect }))
  const layout = await frame.copyTo(buf, { rect })
  const plane0 = layout[0]
  if (!plane0) return null
  if (yuv) {
    return decodeLumaAt('bottom-right', buf.subarray(plane0.offset), rect.width, rect.height, plane0.stride)
  }
  const luma = lumaFromRgba(buf.subarray(plane0.offset), rect.width, rect.height, plane0.stride, rgb)
  return decodeLumaAt('bottom-right', luma, rect.width, rect.height, rect.width)
}
