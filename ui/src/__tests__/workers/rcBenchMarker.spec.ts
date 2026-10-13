// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import {
  BLOCK,
  COLS,
  GRID_H,
  GRID_W,
  MARGIN,
  ROWS,
  crc4,
  decodeLumaAt,
  lumaFromRgba,
  probeRegion,
  word,
  type Corner,
} from '@/workers/rc-bench-marker'

// Limited-range luma of the agent's BGRA 0x00 / 0x80 / 0xFF after the
// BT.601 conversion an encoder applies (`luma_of` in the Rust tests).
const BLACK = 16
const GREY = 126
const WHITE = 235

/** A picture with something under the markers that is NOT grey: the same
 *  kind of horizontal gradient the synthetic source paints. */
function picture(w: number, h: number): Uint8Array {
  const y = new Uint8Array(w * h)
  for (let yy = 0; yy < h; yy++) {
    for (let xx = 0; xx < w; xx++) y[yy * w + xx] = 16 + ((xx * 7 + yy * 3) % 220)
  }
  return y
}

/** Paint `seq` the way `marker::paint_bgra_at` does, straight into luma. */
function paint(y: Uint8Array, w: number, h: number, corner: Corner, seq: number): void {
  const ox = corner === 'bottom-left' ? MARGIN : w - MARGIN - GRID_W
  const oy = h - MARGIN - GRID_H
  const bits = word(seq)
  for (let yy = oy - MARGIN; yy < oy + GRID_H + MARGIN; yy++) {
    for (let xx = ox - MARGIN; xx < ox + GRID_W + MARGIN; xx++) {
      const inside = xx >= ox && xx < ox + GRID_W && yy >= oy && yy < oy + GRID_H
      let v = GREY
      if (inside) {
        const bit = Math.floor((yy - oy) / BLOCK) * COLS + Math.floor((xx - ox) / BLOCK)
        v = (bits >> (15 - bit)) & 1 ? WHITE : BLACK
      }
      y[yy * w + xx] = v
    }
  }
}

/** A k× box downscale, as the agent's resampler or the browser's scaler
 *  would roughly do. */
function downscale(y: Uint8Array, w: number, h: number, k: number): Uint8Array {
  const dw = w / k
  const dh = h / k
  const out = new Uint8Array(dw * dh)
  for (let yy = 0; yy < dh; yy++) {
    for (let xx = 0; xx < dw; xx++) {
      let sum = 0
      for (let j = 0; j < k; j++) for (let i = 0; i < k; i++) sum += y[(yy * k + j) * w + xx * k + i]!
      out[yy * dw + xx] = Math.round(sum / (k * k))
    }
  }
  return out
}

/** Cut `rect` out of a luma plane, as `VideoFrame.copyTo({ rect })` does. */
function crop(
  y: Uint8Array,
  stride: number,
  r: { x: number; y: number; width: number; height: number },
): Uint8Array {
  const out = new Uint8Array(r.width * r.height)
  for (let yy = 0; yy < r.height; yy++) {
    out.set(y.subarray((r.y + yy) * stride + r.x, (r.y + yy) * stride + r.x + r.width), yy * r.width)
  }
  return out
}

describe('rc-bench-marker (the browser half of synthetic_backend::marker)', () => {
  it('has the reference geometry', () => {
    expect([BLOCK, COLS, ROWS, MARGIN, GRID_W, GRID_H]).toEqual([16, 8, 2, 8, 128, 32])
  })

  it('computes the same CRC words the agent paints', () => {
    // `the_words_the_browser_decoder_pins_too` in synthetic_backend.rs pins
    // the same table: if either side drifts, one of the two suites fails.
    const table: Array<[number, number]> = [
      [0x000, 0x0000],
      [0x001, 0x0013],
      [0x800, 0x8001],
      [0x123, 0x1230],
      [0x5a5, 0x5a5d],
      [0xabc, 0xabca],
      [0xfff, 0xfffe],
    ]
    for (const [seq, w] of table) expect(word(seq), `seq ${seq.toString(16)}`).toBe(w)
    // A 1-bit error anywhere in the sequence changes the CRC.
    for (const s of [0x000, 0x123, 0xabc, 0xfff]) {
      for (let b = 0; b < 12; b++) expect(crc4(s)).not.toBe(crc4(s ^ (1 << b)))
    }
  })

  it('reads both corners back, each without disturbing the other', () => {
    const [w, h] = [320, 240]
    const y = picture(w, h)
    paint(y, w, h, 'bottom-left', 0x123)
    paint(y, w, h, 'bottom-right', 0x5a5)
    expect(decodeLumaAt('bottom-left', y, w, h, w)).toBe(0x123)
    expect(decodeLumaAt('bottom-right', y, w, h, w)).toBe(0x5a5)
  })

  it('reads nothing where nothing was painted: never a stale or invented number', () => {
    const [w, h] = [320, 240]
    const y = picture(w, h)
    paint(y, w, h, 'bottom-left', 0x123)
    expect(decodeLumaAt('bottom-right', y, w, h, w)).toBeNull()
  })

  it('survives a 2× and a 4× downscale', () => {
    const [w, h] = [1280, 720]
    const y = picture(w, h)
    paint(y, w, h, 'bottom-right', 0xabc)
    for (const k of [2, 4]) {
      expect(decodeLumaAt('bottom-right', downscale(y, w, h, k), w / k, h / k, w / k), `k=${k}`).toBe(0xabc)
    }
  })

  it('decodes the copied region as a picture of its own, at every scale', () => {
    const [w, h] = [1280, 720]
    const y = picture(w, h)
    paint(y, w, h, 'bottom-right', 0x7f1)
    for (const k of [1, 2, 4]) {
      const [dw, dh] = [w / k, h / k]
      const plane = k === 1 ? y : downscale(y, w, h, k)
      const r = probeRegion(dw, dh)!
      expect(r.x % 2, 'even for a 4:2:0 copy').toBe(0)
      expect(r.y % 2).toBe(0)
      expect(r.x + r.width).toBe(dw)
      expect(r.y + r.height).toBe(dh)
      expect(decodeLumaAt('bottom-right', crop(plane, dw, r), r.width, r.height, r.width), `k=${k}`).toBe(0x7f1)
    }
  })

  it('honours a visible rect that does not start at the origin', () => {
    // A frame whose coded area is larger than what is shown (an encoder's
    // alignment crop): the region is cut from the VISIBLE picture's corner.
    const [cw, ch] = [336, 256]
    const vis = { x: 8, y: 6, w: 320, h: 240 }
    const coded = picture(cw, ch)
    const visible = crop(coded, cw, { x: vis.x, y: vis.y, width: vis.w, height: vis.h })
    paint(visible, vis.w, vis.h, 'bottom-right', 0x0f0)
    for (let yy = 0; yy < vis.h; yy++) coded.set(visible.subarray(yy * vis.w, (yy + 1) * vis.w), (vis.y + yy) * cw + vis.x)
    const r = probeRegion(vis.w, vis.h, vis.x, vis.y)!
    expect(r.x + r.width).toBe(vis.x + vis.w)
    expect(r.y + r.height).toBe(vis.y + vis.h)
    expect(decodeLumaAt('bottom-right', crop(coded, cw, r), r.width, r.height, r.width)).toBe(0x0f0)
  })

  it('turns a flipped block into a miss, never a wrong number', () => {
    const [w, h] = [320, 240]
    const ox = w - MARGIN - GRID_W
    const oy = h - MARGIN - GRID_H
    for (let bit = 0; bit < ROWS * COLS; bit++) {
      const y = picture(w, h)
      paint(y, w, h, 'bottom-right', 0x5a5)
      const bx = ox + (bit % COLS) * BLOCK
      const by = oy + Math.floor(bit / COLS) * BLOCK
      for (let yy = by; yy < by + BLOCK; yy++) {
        for (let xx = bx; xx < bx + BLOCK; xx++) y[yy * w + xx] = y[yy * w + xx] === WHITE ? BLACK : WHITE
      }
      // One flipped bit is one CRC mismatch, whichever half of the word it hits.
      expect(decodeLumaAt('bottom-right', y, w, h, w), `bit ${bit}`).toBeNull()
    }
  })

  it('refuses a block that averages inside the dead band', () => {
    const [w, h] = [320, 240]
    const y = picture(w, h)
    paint(y, w, h, 'bottom-right', 0x5a5)
    const ox = w - MARGIN - GRID_W
    const oy = h - MARGIN - GRID_H
    for (let yy = oy; yy < oy + BLOCK; yy++) for (let xx = ox; xx < ox + BLOCK; xx++) y[yy * w + xx] = 130
    expect(decodeLumaAt('bottom-right', y, w, h, w)).toBeNull()
  })

  it('refuses a picture too small to hold the region', () => {
    expect(probeRegion(143, 240)).toBeNull()
    expect(probeRegion(320, 47)).toBeNull()
    expect(decodeLumaAt('bottom-right', new Uint8Array(100 * 40), 100, 40, 100)).toBeNull()
  })

  it('reads a canvas readback (full-range RGBA) and a BGRA frame through lumaFromRgba', () => {
    const [w, h] = [320, 240]
    const y = picture(w, h)
    paint(y, w, h, 'bottom-right', 0x321)
    // What a browser's YUV → RGB conversion hands back: full range.
    const full = (v: number) => Math.max(0, Math.min(255, Math.round(((v - 16) * 255) / 219)))
    for (const order of ['rgba', 'bgra'] as const) {
      const px = new Uint8ClampedArray(w * h * 4)
      for (let i = 0; i < w * h; i++) {
        const v = full(y[i]!)
        px.set([v, v, v, 255], i * 4)
      }
      expect(decodeLumaAt('bottom-right', lumaFromRgba(px, w, h, w * 4, order), w, h, w), order).toBe(0x321)
    }
  })
})
