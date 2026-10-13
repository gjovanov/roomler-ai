// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import { BENCH_ARM_TTL_MS, createBenchSampler, readProbe, type BenchProbeMessage } from '@/workers/rc-bench-sampler'
import { GRID_H, GRID_W, MARGIN, BLOCK, COLS, word } from '@/workers/rc-bench-marker'

/** The smallest VideoFrame stand-in the sampler needs: clone and close. */
function frame(id: number) {
  const state = { id, closed: 0, clones: 0 }
  const f = {
    state,
    clone() {
      state.clones++
      return frame(id * 10 + state.clones)
    },
    close() {
      state.closed++
    },
  }
  return f as unknown as VideoFrame & { state: typeof state }
}

const flush = () => new Promise((r) => setTimeout(r, 0))

describe('rc-bench-sampler (the decode worker half)', () => {
  it('reads nothing until enabled AND armed', () => {
    const s = createBenchSampler({ post: () => {}, now: () => 0, read: async () => 1 })
    expect(s.take(frame(1))).toBeNull()
    s.arm(1) // ignored: not enabled
    expect(s.armedCount()).toBe(0)
    s.setEnabled(true)
    expect(s.take(frame(1))).toBeNull()
    s.arm(1)
    expect(s.take(frame(1))).not.toBeNull()
  })

  it('answers an armed probe once, from the first painted frame carrying it', async () => {
    const posts: BenchProbeMessage[] = []
    const reads = [3, 7, 7]
    const s = createBenchSampler({ post: (m) => posts.push(m), now: () => 0, read: async () => reads.shift() ?? null })
    s.setEnabled(true)
    s.arm(7)
    for (const paintedAt of [100, 117, 133]) {
      const c = s.take(frame(paintedAt))
      if (c) s.settle(c, paintedAt)
      await flush()
    }
    expect(posts).toEqual([{ type: 'bench-probe', seq: 7, paintedAt: 117 }])
    expect(s.armedCount()).toBe(0)
  })

  it('settles reads in paint order even when a later read finishes first', async () => {
    const posts: BenchProbeMessage[] = []
    const resolvers: Array<(v: number | null) => void> = []
    const s = createBenchSampler({
      post: (m) => posts.push(m),
      now: () => 0,
      read: () => new Promise<number | null>((r) => resolvers.push(r)),
    })
    s.setEnabled(true)
    s.arm(9)
    const a = s.take(frame(1))!
    s.settle(a, 100)
    const b = s.take(frame(2))!
    s.settle(b, 117)
    await flush()
    // Only the first read has started: the chain runs one at a time.
    expect(resolvers).toHaveLength(1)
    resolvers[0]!(9)
    await flush()
    await flush()
    expect(posts).toEqual([{ type: 'bench-probe', seq: 9, paintedAt: 100 }])
  })

  it('closes every clone it took, a failed read included', async () => {
    const s = createBenchSampler({
      post: () => {},
      now: () => 0,
      read: async () => {
        throw new Error('copyTo refused')
      },
    })
    s.setEnabled(true)
    s.arm(1)
    const c = s.take(frame(1)) as VideoFrame & { state: { closed: number } }
    s.settle(c, 0)
    await flush()
    await flush()
    expect(c.state.closed).toBe(1)
  })

  it('holds at most two frames at once', () => {
    const s = createBenchSampler({ post: () => {}, now: () => 0, read: () => new Promise(() => {}) })
    s.setEnabled(true)
    s.arm(1)
    const a = s.take(frame(1))!
    s.settle(a, 0)
    const b = s.take(frame(2))!
    s.settle(b, 0)
    expect(s.take(frame(3))).toBeNull()
  })

  it('lets an unanswered probe expire, so a lost probe stops the readbacks', () => {
    let now = 0
    const s = createBenchSampler({ post: () => {}, now: () => now, read: async () => null })
    s.setEnabled(true)
    s.arm(4)
    now = BENCH_ARM_TTL_MS + 1
    expect(s.take(frame(1))).toBeNull()
    expect(s.armedCount()).toBe(0)
  })

  it('disarms everything when bench mode is switched off', () => {
    const s = createBenchSampler({ post: () => {}, now: () => 0, read: async () => null })
    s.setEnabled(true)
    s.arm(1)
    s.arm(2)
    s.setEnabled(false)
    expect(s.armedCount()).toBe(0)
  })
})

/** A decoded picture with the probe painted bottom-right, as an I420 or
 *  RGBA VideoFrame whose copyTo honours `rect` the way the browser does. */
function decodedFrame(seq: number, fmt: 'I420' | 'RGBA') {
  const [w, h] = [320, 240]
  const y = new Uint8Array(w * h).fill(60)
  const ox = w - MARGIN - GRID_W
  const oy = h - MARGIN - GRID_H
  const bits = word(seq)
  for (let yy = oy - MARGIN; yy < oy + GRID_H + MARGIN; yy++) {
    for (let xx = ox - MARGIN; xx < ox + GRID_W + MARGIN; xx++) {
      const inside = xx >= ox && xx < ox + GRID_W && yy >= oy && yy < oy + GRID_H
      const bit = Math.floor((yy - oy) / BLOCK) * COLS + Math.floor((xx - ox) / BLOCK)
      y[yy * w + xx] = !inside ? 126 : (bits >> (15 - bit)) & 1 ? 235 : 16
    }
  }
  const bpp = fmt === 'RGBA' ? 4 : 1
  const rects: Array<{ x: number; y: number; width: number; height: number }> = []
  return {
    rects,
    frame: {
      format: fmt,
      visibleRect: { x: 0, y: 0, width: w, height: h },
      allocationSize: ({ rect }: { rect: { width: number; height: number } }) =>
        fmt === 'RGBA' ? rect.width * rect.height * 4 : (rect.width * rect.height * 3) / 2,
      copyTo: async (buf: Uint8Array, { rect }: { rect: { x: number; y: number; width: number; height: number } }) => {
        rects.push(rect)
        for (let yy = 0; yy < rect.height; yy++) {
          for (let xx = 0; xx < rect.width; xx++) {
            const v = y[(rect.y + yy) * w + rect.x + xx]!
            if (bpp === 1) buf[yy * rect.width + xx] = v
            else buf.set([v, v, v, 255], (yy * rect.width + xx) * 4)
          }
        }
        return [{ offset: 0, stride: rect.width * bpp }]
      },
    } as unknown as VideoFrame,
  }
}

describe('readProbe', () => {
  for (const fmt of ['I420', 'RGBA'] as const) {
    it(`reads the probe from a ${fmt} frame's bottom-right region only`, async () => {
      const { frame: f, rects } = decodedFrame(0x2a7, fmt)
      expect(await readProbe(f)).toBe(0x2a7)
      // The copy is the small corner region, never the whole picture.
      expect(rects).toEqual([{ x: 320 - 144, y: 240 - 48, width: 144, height: 48 }])
    })
  }

  it('reads nothing from a format with no 8-bit luma', async () => {
    const f = { format: 'I420P10', visibleRect: { x: 0, y: 0, width: 320, height: 240 } } as unknown as VideoFrame
    expect(await readProbe(f)).toBeNull()
  })
})
