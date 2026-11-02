import { describe, expect, it } from 'vitest'
import {
  VIEWER_HZ_EVERY_MS,
  VIEWER_HZ_INTERVALS,
  VIEWER_HZ_MAX_GAP_MS,
  createHzMeter,
  startViewerHzBursts,
} from '@/workers/rc-viewer-hz'

/** Feed a meter ticks `intervalMs` apart until it reads, or give up. */
function read(intervalAt: (i: number) => number, ticks = VIEWER_HZ_INTERVALS + 1): number | null {
  const meter = createHzMeter()
  let t = 1000
  for (let i = 0; i < ticks; i++) {
    const hz = meter.tick(t)
    if (hz !== null) return hz
    t += intervalAt(i)
  }
  return null
}

describe('FR-135 R2: the refresh meter', () => {
  it.each([
    [60, 1000 / 60],
    [60, 1000 / 59.94],
    [75, 1000 / 75],
    [90, 1000 / 90],
    [120, 1000 / 119.88],
    [144, 1000 / 144],
    [240, 1000 / 240],
  ])('reads %i Hz from an interval of %f ms', (hz, ms) => {
    expect(read(() => ms)).toBe(hz)
  })

  it('reads between a coarse clock\'s ticks, as Firefox\'s rAF has them', () => {
    // 16, 17, 17 ms in turn is 60 Hz. A median alone reads 59.
    expect(read((i) => (i % 3 === 0 ? 16 : 17))).toBe(60)
  })

  it('is not moved by ticks the thread missed', () => {
    // One in five is a double interval: the screen is still 60 Hz.
    expect(read((i) => (i % 5 === 0 ? (2 * 1000) / 60 : 1000 / 60))).toBe(60)
  })

  it('starts over after a gap, because a hidden tab is not a slow screen', () => {
    const meter = createHzMeter()
    let t = 0
    let got: number | null = null
    for (let i = 0; i < VIEWER_HZ_INTERVALS - 1; i++) {
      t += 16
      got = meter.tick(t) ?? got
    }
    t += VIEWER_HZ_MAX_GAP_MS + 1
    got = meter.tick(t) ?? got
    t += 16
    got = meter.tick(t) ?? got
    expect(got).toBeNull()
    for (let i = 0; i < VIEWER_HZ_INTERVALS; i++) {
      t += 10
      got = meter.tick(t) ?? got
    }
    expect(got).toBe(100)
  })

  it('ignores a clock that did not move', () => {
    const meter = createHzMeter()
    expect(meter.tick(5)).toBeNull()
    expect(meter.tick(5)).toBeNull()
  })

  it('reads no more than one number per window', () => {
    const meter = createHzMeter()
    let t = 0
    const readings: number[] = []
    for (let i = 0; i <= VIEWER_HZ_INTERVALS * 3; i++) {
      const hz = meter.tick(t)
      if (hz !== null) readings.push(hz)
      t += 10
    }
    expect(readings).toEqual([100, 100, 100])
  })
})

/** A hand-cranked rAF and timer. */
function fakeEnv() {
  let pending: ((t: number) => void) | null = null
  const timers: { run: () => void; ms: number }[] = []
  let now = 0
  return {
    env: {
      raf: (cb: (t: number) => void) => {
        pending = cb
      },
      later: (run: () => void, ms: number) => {
        timers.push({ run, ms })
      },
    },
    timers,
    /** Run rAF ticks until it stops asking, or `max`. */
    ticks(intervalMs: number, max = 1000) {
      let n = 0
      while (pending && n < max) {
        const cb = pending
        pending = null
        now += intervalMs
        cb(now)
        n++
      }
      return n
    },
    get waiting() {
      return pending !== null
    },
  }
}

describe('FR-135 R2: the bursts', () => {
  it('measures, reports, and lets the thread rest until the next burst', () => {
    const f = fakeEnv()
    const told: number[] = []
    startViewerHzBursts((hz) => told.push(hz), f.env)
    f.ticks(1000 / 60)
    expect(told).toEqual([60])
    expect(f.waiting).toBe(false)
    expect(f.timers).toHaveLength(1)
    expect(f.timers[0]!.ms).toBe(VIEWER_HZ_EVERY_MS)
  })

  it('says nothing when the next burst reads the same screen', () => {
    const f = fakeEnv()
    const told: number[] = []
    startViewerHzBursts((hz) => told.push(hz), f.env)
    f.ticks(1000 / 144)
    f.timers.shift()!.run()
    f.ticks(1000 / 143.4)
    expect(told).toEqual([144])
    expect(f.timers).toHaveLength(1)
  })

  it('reports a window moved to another screen', () => {
    const f = fakeEnv()
    const told: number[] = []
    startViewerHzBursts((hz) => told.push(hz), f.env)
    f.ticks(1000 / 144)
    f.timers.shift()!.run()
    f.ticks(1000 / 60)
    expect(told).toEqual([144, 60])
  })

  it('does nothing on a browser whose workers have no rAF', () => {
    const told: number[] = []
    const later: unknown[] = []
    startViewerHzBursts((hz) => told.push(hz), { later: (run) => later.push(run) })
    expect(told).toEqual([])
    expect(later).toEqual([])
  })
})
