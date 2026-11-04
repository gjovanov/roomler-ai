// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-124 — the black-picture detector's rules.
//
// The thresholds are the claim: a covered webcam measured mean luma 0 and peak
// 1–3 over a 16×12 sample (2026-10-26), while a dim room under auto-exposure
// and a dark screen with text both read far brighter. A threshold that drifts
// either way is a covered camera nobody is told about, or a dim room told its
// lens is covered.
import { describe, it, expect, vi, afterEach } from 'vitest'
import {
  BLACK_SAMPLES_TO_FLAG,
  BlackPictureTracker,
  isBlackPicture,
  lumaStats,
  sampleLuma,
  watchBlackPicture,
  type LumaStats,
} from '@/utils/blackPicture'

/** An RGBA buffer of `n` pixels, every one `rgb`. */
function fill(n: number, rgb: [number, number, number]): Uint8ClampedArray {
  const a = new Uint8ClampedArray(n * 4)
  for (let i = 0; i < n; i++) a.set([...rgb, 255], i * 4)
  return a
}

afterEach(() => {
  vi.useRealTimers()
  vi.restoreAllMocks()
})

describe('lumaStats', () => {
  it('reads a black buffer as 0 / 0', () => {
    expect(lumaStats(fill(192, [0, 0, 0]))).toEqual({ mean: 0, peak: 0 })
  })

  it('weighs the channels the way the eye does (BT.601)', () => {
    const s = lumaStats(fill(4, [0, 255, 0]))
    expect(s.mean).toBeCloseTo(0.587 * 255, 5)
    expect(s.peak).toBeCloseTo(0.587 * 255, 5)
  })

  it('takes the brightest sample as the peak', () => {
    const a = fill(192, [0, 0, 0])
    a.set([255, 255, 255, 255], 100 * 4)
    const s = lumaStats(a)
    expect(s.peak).toBeCloseTo(255, 5)
    expect(s.mean).toBeCloseTo(255 / 192, 5)
  })

  it('reads an empty buffer as 0 rather than NaN', () => {
    expect(lumaStats(new Uint8ClampedArray(0))).toEqual({ mean: 0, peak: 0 })
  })
})

describe('isBlackPicture', () => {
  it.each<[string, LumaStats, boolean]>([
    ['a covered lens, as measured (mean 0, peak 3)', { mean: 0, peak: 3 }, true],
    ['the edge of the bar (mean 4, peak 16)', { mean: 4, peak: 16 }, true],
    ['a dim room under auto-exposure (mean 12, peak 40)', { mean: 12, peak: 40 }, false],
    ['a dark screen with text (mean 15, peak 255)', { mean: 15, peak: 255 }, false],
    ['black but for one bright spot (mean 1.3, peak 255)', { mean: 1.3, peak: 255 }, false],
    ['just over the mean (mean 4.1, peak 10)', { mean: 4.1, peak: 10 }, false],
  ])('%s → %s', (_name, stats, black) => {
    expect(isBlackPicture(stats)).toBe(black)
  })
})

describe('BlackPictureTracker', () => {
  it(`calls a picture black only after ${BLACK_SAMPLES_TO_FLAG} black samples in a row`, () => {
    const t = new BlackPictureTracker()
    for (let i = 1; i < BLACK_SAMPLES_TO_FLAG; i++) expect(t.push(true)).toBe(false)
    expect(t.push(true)).toBe(true)
    expect(t.push(true)).toBe(true)
  })

  it('clears at the first sample with a picture in it', () => {
    const t = new BlackPictureTracker(2)
    t.push(true)
    expect(t.push(true)).toBe(true)
    expect(t.push(false)).toBe(false)
    expect(t.push(true)).toBe(false)
  })

  it('nothing to judge (camera off, not playing) clears and starts the run again', () => {
    const t = new BlackPictureTracker(3)
    t.push(true)
    t.push(true)
    expect(t.push(null)).toBe(false)
    t.push(true)
    expect(t.push(true)).toBe(false)
    expect(t.push(true)).toBe(true)
    expect(t.push(null)).toBe(false)
  })
})

describe('sampleLuma — no frame, no verdict', () => {
  const ctx = {
    drawImage: vi.fn(),
    getImageData: vi.fn(() => ({ data: fill(192, [0, 0, 0]) })),
  } as unknown as CanvasRenderingContext2D

  function video(over: Partial<HTMLVideoElement>): HTMLVideoElement {
    return { paused: false, ended: false, readyState: 4, videoWidth: 640, videoHeight: 480, ...over } as HTMLVideoElement
  }

  it.each<[string, Partial<HTMLVideoElement>]>([
    ['paused', { paused: true }],
    ['ended', { ended: true }],
    ['no frame yet (readyState 1)', { readyState: 1 }],
    ['no size yet', { videoWidth: 0 }],
  ])('%s → null', (_name, over) => {
    expect(sampleLuma(video(over), ctx)).toBeNull()
  })

  it('a playing frame is sampled at 16×12', () => {
    expect(sampleLuma(video({}), ctx)).toEqual({ mean: 0, peak: 0 })
    expect(ctx.drawImage).toHaveBeenCalledWith(expect.anything(), 0, 0, 16, 12)
  })

  it('a frame the canvas refuses gives no verdict instead of a guess', () => {
    const bad = {
      drawImage: () => {
        throw new DOMException('tainted', 'SecurityError')
      },
      getImageData: vi.fn(),
    } as unknown as CanvasRenderingContext2D
    expect(sampleLuma(video({}), bad)).toBeNull()
  })
})

describe('watchBlackPicture', () => {
  const black: LumaStats = { mean: 0, peak: 2 }
  const picture: LumaStats = { mean: 120, peak: 240 }
  const el = {} as HTMLVideoElement

  it('reports black after three seconds of black, and the picture coming back', () => {
    vi.useFakeTimers()
    let s: LumaStats = black
    const onChange = vi.fn()
    const stop = watchBlackPicture(el, { isActive: () => true, onChange, sample: () => s })

    vi.advanceTimersByTime(2000)
    expect(onChange).not.toHaveBeenCalled()
    vi.advanceTimersByTime(1000)
    expect(onChange).toHaveBeenLastCalledWith(true)

    vi.advanceTimersByTime(5000)
    expect(onChange).toHaveBeenCalledTimes(1) // changes only, not every tick

    s = picture
    vi.advanceTimersByTime(1000)
    expect(onChange).toHaveBeenLastCalledWith(false)
    stop()
  })

  it('never judges while inactive (your camera turned off, a peer who said it is off)', () => {
    vi.useFakeTimers()
    const onChange = vi.fn()
    const sample = vi.fn(() => black)
    const stop = watchBlackPicture(el, { isActive: () => false, onChange, sample })
    vi.advanceTimersByTime(10_000)
    expect(onChange).not.toHaveBeenCalled()
    expect(sample).not.toHaveBeenCalled()
    stop()
  })

  it('going inactive while black clears the verdict', () => {
    vi.useFakeTimers()
    let active = true
    const onChange = vi.fn()
    const stop = watchBlackPicture(el, { isActive: () => active, onChange, sample: () => black })
    vi.advanceTimersByTime(3000)
    expect(onChange).toHaveBeenLastCalledWith(true)
    active = false
    vi.advanceTimersByTime(1000)
    expect(onChange).toHaveBeenLastCalledWith(false)
    stop()
  })

  it('a hidden page keeps its verdict and takes no samples', () => {
    vi.useFakeTimers()
    const hidden = vi.spyOn(document, 'hidden', 'get').mockReturnValue(true)
    const onChange = vi.fn()
    const sample = vi.fn(() => black)
    const stop = watchBlackPicture(el, { isActive: () => true, onChange, sample })
    vi.advanceTimersByTime(10_000)
    expect(sample).not.toHaveBeenCalled()
    expect(onChange).not.toHaveBeenCalled()
    hidden.mockRestore()
    stop()
  })

  it('stop() while black reports false once, and stops sampling', () => {
    vi.useFakeTimers()
    const onChange = vi.fn()
    const sample = vi.fn(() => black)
    const stop = watchBlackPicture(el, { isActive: () => true, onChange, sample })
    vi.advanceTimersByTime(3000)
    stop()
    expect(onChange.mock.calls).toEqual([[true], [false]])
    const calls = sample.mock.calls.length
    vi.advanceTimersByTime(5000)
    expect(sample.mock.calls.length).toBe(calls)
  })

  it('stop() while not black reports nothing', () => {
    vi.useFakeTimers()
    const onChange = vi.fn()
    const stop = watchBlackPicture(el, { isActive: () => true, onChange, sample: () => picture })
    vi.advanceTimersByTime(3000)
    stop()
    expect(onChange).not.toHaveBeenCalled()
  })
})
