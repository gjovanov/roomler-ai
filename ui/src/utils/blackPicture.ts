// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-124 — is a video element showing a black picture?
//
// A covered lens, a closed privacy shutter, or a camera-off key that blanks the
// sensor while the device stays attached all look the same from here: a stream
// that plays, at a normal size, with nothing in it. Nothing else on screen said
// so, and on 2026-10-26 a covered webcam on one device was reported as the
// OTHER device's camera not working: both were signed in to one account, so
// both tiles carried the same name.
//
// Measured on that covered webcam: mean luma 0 and peak 1–3 (of 255) over a
// 16×12 sample. A dim room under auto-exposure reads far brighter, and a dark
// screen with text has bright peaks, so the bar is "no light at all", not "dim".
// The samples never leave the tab: nothing goes on the wire.

/** The sample grid: 192 points, averaged by the browser's own downscale. */
export const SAMPLE_W = 16
export const SAMPLE_H = 12
/** Black = a mean luma of at most this… */
export const BLACK_MEAN_MAX = 4
/** …and no sample brighter than this. */
export const BLACK_PEAK_MAX = 16
/** Black samples in a row before the picture is called black. One a second, so
 *  about 3 s: long enough for a camera's first dark frames while its exposure
 *  settles, short enough to say it before anyone starts debugging. */
export const BLACK_SAMPLES_TO_FLAG = 3
export const SAMPLE_INTERVAL_MS = 1000

export interface LumaStats {
  mean: number
  peak: number
}

/** Mean and peak luma (BT.601 weights) of an RGBA buffer, as `getImageData` returns it. */
export function lumaStats(rgba: ArrayLike<number>): LumaStats {
  let sum = 0
  let peak = 0
  let n = 0
  for (let i = 0; i + 2 < rgba.length; i += 4) {
    const l = 0.299 * rgba[i] + 0.587 * rgba[i + 1] + 0.114 * rgba[i + 2]
    sum += l
    if (l > peak) peak = l
    n++
  }
  return { mean: n ? sum / n : 0, peak }
}

export function isBlackPicture(s: LumaStats): boolean {
  return s.mean <= BLACK_MEAN_MAX && s.peak <= BLACK_PEAK_MAX
}

/**
 * Hysteresis over the samples: black after `samplesToFlag` black samples in a
 * row, not black at the first sample with a picture in it. `null` means there
 * is nothing to judge (not playing, the camera turned off) and starts the run
 * again, so a camera that is OFF is never reported as a covered one.
 */
export class BlackPictureTracker {
  private run = 0
  black = false

  constructor(private readonly samplesToFlag = BLACK_SAMPLES_TO_FLAG) {}

  push(sample: boolean | null): boolean {
    if (sample === true) {
      this.run += 1
      if (this.run >= this.samplesToFlag) this.black = true
    } else {
      this.run = 0
      this.black = false
    }
    return this.black
  }
}

/** One sample of what `video` shows right now, or `null` when it has no frame to judge. */
export function sampleLuma(video: HTMLVideoElement, ctx: CanvasRenderingContext2D): LumaStats | null {
  // HAVE_CURRENT_DATA (2): a frame exists. A paused or not-yet-started element
  // shows nothing, and that is not the camera's doing.
  if (video.paused || video.ended || video.readyState < 2 || !video.videoWidth || !video.videoHeight) {
    return null
  }
  try {
    ctx.drawImage(video, 0, 0, SAMPLE_W, SAMPLE_H)
    return lumaStats(ctx.getImageData(0, 0, SAMPLE_W, SAMPLE_H).data)
  } catch {
    // A frame the canvas can't take: say nothing rather than guess.
    return null
  }
}

export interface WatchBlackPictureOptions {
  /** Read on every tick: may this picture be judged right now? */
  isActive: () => boolean
  /** Called when the verdict flips, and with `false` on stop if it was black. */
  onChange: (black: boolean) => void
  intervalMs?: number
  /** Test seam: replaces the canvas sample. */
  sample?: (video: HTMLVideoElement) => LumaStats | null
}

/**
 * Samples `video` once a second and reports when its picture turns black, and
 * when it stops being black. A hidden page skips its samples and keeps its
 * verdict. Returns the stop function.
 */
export function watchBlackPicture(video: HTMLVideoElement, opts: WatchBlackPictureOptions): () => void {
  const tracker = new BlackPictureTracker()
  // Made on the first sample that needs it: a tile that is never judged (no
  // stream, camera off) never creates a canvas.
  let ctx: CanvasRenderingContext2D | null | undefined
  const sample =
    opts.sample ??
    ((v: HTMLVideoElement) => {
      if (ctx === undefined) {
        const canvas = document.createElement('canvas')
        canvas.width = SAMPLE_W
        canvas.height = SAMPLE_H
        ctx = canvas.getContext('2d', { willReadFrequently: true })
      }
      return ctx ? sampleLuma(v, ctx) : null
    })

  let last = false
  const timer = setInterval(() => {
    if (typeof document !== 'undefined' && document.hidden) return
    let verdict: boolean | null = null
    if (opts.isActive()) {
      const s = sample(video)
      verdict = s ? isBlackPicture(s) : null
    }
    const black = tracker.push(verdict)
    if (black !== last) {
      last = black
      opts.onChange(black)
    }
  }, opts.intervalMs ?? SAMPLE_INTERVAL_MS)

  return () => {
    clearInterval(timer)
    if (last) {
      last = false
      opts.onChange(false)
    }
  }
}
