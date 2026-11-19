// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-135 N11 (#3875) — the freeze rule: a paint gap is a freeze only by what
 * the viewer added to the host's own longest pause between the two painted
 * frames. Times are the worker's ms; a frame's wire timestamp is the agent's
 * µs at framing, so each case writes the host's framing in µs and the paints
 * in ms, and the rule only ever compares an interval on one clock with an
 * interval on the other.
 */
import { describe, it, expect } from 'vitest'
import {
  FREEZE_GAP_MS,
  MAX_FRAMED,
  createFreezeCounter,
  isFreeze,
  type FreezeCounter,
} from '@/workers/rc-freeze'

/** µs in a ms: the wire's timestamps are µs. */
const MS = 1000

/** The host framed a frame every `everyMs` from `fromMs` up to `toMs`, in µs. */
function cadence(fromMs: number, toMs: number, everyMs: number): number[] {
  const out: number[] = []
  for (let t = fromMs; t <= toMs; t += everyMs) out.push(Math.round(t * MS))
  return out
}

/** Frame and paint `ts` (µs) with the pipeline's latency `lagMs`, as a healthy
 *  session does: every frame the host framed reaches the screen. */
function onTime(f: FreezeCounter, ts: number[], lagMs: number): void {
  for (const t of ts) {
    f.framed(t)
    f.painted(t / MS + lagMs, t)
  }
}

describe('FR-135 N11 — the verdict', () => {
  it(`is the paint gap less the host's pause, over ${FREEZE_GAP_MS} ms`, () => {
    expect(isFreeze(401, 200)).toBe(true)
    expect(isFreeze(400, 200)).toBe(false)
    expect(isFreeze(FREEZE_GAP_MS + 0.5, 0)).toBe(true)
    expect(isFreeze(FREEZE_GAP_MS, 0)).toBe(false)
    expect(isFreeze(5_000, 4_900)).toBe(false)
  })
})

describe('FR-135 N11 — a gap the host left is no freeze', () => {
  it('when motion stops and the host frames nothing for a second', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(0, 500, 16), 30)
    // The screen went still: the next frame is framed a second later and
    // paints at the same latency, so the picture waited for the host alone.
    f.framed(1_500 * MS)
    expect(f.painted(1_530, 1_500 * MS)).toBe(false)
    onTime(f, cadence(1_516, 2_000, 16), 30)
    expect(f.count).toBe(0)
  })

  it('when a still screen sends a sparse frame, a caret blinking every 530 ms', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(0, 5_300, 530), 25)
    expect(f.count).toBe(0)
  })

  it('when the host frames 4 times a second', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(0, 10_000, 250), 40)
    expect(f.count).toBe(0)
  })

  it('when the host stalls (an encoder open), with no frame dropped on the way', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(0, 400, 16), 30)
    // 1.6 s with nothing framed, then the next frame at its usual latency.
    onTime(f, cadence(2_000, 2_400, 16), 30)
    expect(f.count).toBe(0)
  })
})

describe('FR-135 N11 — a gap the viewer made is one', () => {
  it('when the path stalls while the host frames steadily', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(0, 400, 16), 30)
    // Frames 416… are framed on time but held on the path for 400 ms, then
    // all arrive and paint in a burst: one freeze, not one per frame.
    const held = cadence(416, 816, 16)
    for (const t of held) f.framed(t)
    for (const [i, t] of held.entries()) f.painted(400 + 30 + 400 + i * 0.5, t)
    expect(f.count).toBe(1)
  })

  it('when the decoder holds the frames that arrived', () => {
    const f = createFreezeCounter()
    // The frames are framed, and reach the worker, ahead of their paint.
    const ts = cadence(0, 1_000, 16)
    for (const t of ts.slice(0, 3)) f.framed(t)
    f.painted(30, ts[0]!)
    for (const t of ts.slice(3)) f.framed(t)
    // The decoder said nothing for 500 ms, then caught up.
    for (const [i, t] of ts.slice(1).entries()) f.painted(530 + i, t)
    expect(f.count).toBe(1)
  })

  it('when the key-frame gate dropped the frames the host kept framing', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(0, 400, 16), 30)
    // A loss closes the gate: the deltas 416…800 reach the worker and are
    // dropped, and the key frame framed at 816 paints at 866. The picture
    // waited 436 ms; the host never paused more than 16.
    for (const t of cadence(416, 816, 16)) f.framed(t)
    expect(f.painted(866, 816 * MS)).toBe(true)
    // The host's gap between the two painted frames is 416 ms, so a rule that
    // read only the painted frames' own timestamps would call this no freeze.
    expect(f.count).toBe(1)
  })

  it('when a still screen is kept alive every 62 ms and a paint comes 300 ms late', () => {
    // Option (a): a keepalive is counted like any frame. The stall carried no
    // new content, so nobody saw it, but the header cannot say so.
    const f = createFreezeCounter()
    onTime(f, cadence(0, 620, 62), 30)
    f.framed(682 * MS)
    expect(f.painted(620 + 30 + 300, 682 * MS)).toBe(true)
    // 250 ms is within 200 of the keepalive's own 62.
    const g = createFreezeCounter()
    onTime(g, cadence(0, 620, 62), 30)
    g.framed(682 * MS)
    expect(g.painted(620 + 30 + 250, 682 * MS)).toBe(false)
  })
})

describe('FR-135 N11 — mixed and boundary cases', () => {
  it('a host pause the viewer lengthened counts by the viewer’s share alone', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(0, 400, 16), 30)
    // 300 ms with nothing framed, then the frame 150 ms later than its usual
    // latency: 450 ms on the screen, 150 of them the viewer's.
    f.framed(700 * MS)
    expect(f.painted(400 + 30 + 300 + 150, 700 * MS)).toBe(false)
    // The same pause, the frame 250 ms late: the viewer's share is a freeze.
    const g = createFreezeCounter()
    onTime(g, cadence(0, 400, 16), 30)
    g.framed(700 * MS)
    expect(g.painted(400 + 30 + 300 + 250, 700 * MS)).toBe(true)
  })

  it('takes the host’s longest pause, not the sum, when a frame was dropped before it', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(0, 400, 16), 30)
    // 416 is dropped, then the host itself pauses 284 ms (its own stall), and
    // 700 paints 20 ms past its latency: 320 ms on the screen, 284 the host's.
    f.framed(416 * MS)
    f.framed(700 * MS)
    expect(f.painted(400 + 30 + 320, 700 * MS)).toBe(false)
    expect(f.count).toBe(0)
  })

  it(`counts exactly ${FREEZE_GAP_MS} ms past the host's pause as no freeze, and a hair over as one`, () => {
    const at = (extraMs: number) => {
      const f = createFreezeCounter()
      f.framed(0)
      f.painted(1_000, 0)
      f.framed(300 * MS)
      return f.painted(1_000 + 300 + extraMs, 300 * MS)
    }
    expect(at(FREEZE_GAP_MS)).toBe(false)
    expect(at(FREEZE_GAP_MS + 0.5)).toBe(true)
  })

  it('never counts a paint gap of 200 ms or less, whatever the host did', () => {
    const f = createFreezeCounter()
    f.framed(0)
    f.painted(0, 0)
    f.framed(1)
    f.painted(FREEZE_GAP_MS, 1) // the host framed 1 µs later
    expect(f.count).toBe(0)
  })

  it('judges nothing at the first paint', () => {
    const f = createFreezeCounter()
    f.framed(5_000 * MS)
    expect(f.painted(90_000, 5_000 * MS)).toBe(false)
    expect(f.count).toBe(0)
  })

  it('reads a frame painted without its header as framed when it painted', () => {
    // The recorder's own tests paint without telling the counter of headers.
    const f = createFreezeCounter()
    f.painted(0, 0)
    expect(f.painted(400, 1)).toBe(true)
    expect(f.painted(1_500, 1_000_000)).toBe(false)
  })
})

describe('FR-135 N11 — the clock', () => {
  it('restarts at the next paint across a capture hold the agent announced', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(0, 400, 16), 30)
    f.hold()
    // Whatever the frames say, the gap across the hold is not judged.
    f.framed(416 * MS)
    expect(f.painted(9_000, 416 * MS)).toBe(false)
    // Past it the rule runs again.
    f.framed(432 * MS)
    expect(f.painted(9_000 + FREEZE_GAP_MS + 30, 432 * MS)).toBe(true)
  })

  it('restarts unjudged when the wire timestamp runs backwards (another agent process)', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(60_000, 60_400, 16), 30)
    // A frame from a fresh process clock, long after: not judged.
    f.framed(16 * MS)
    expect(f.painted(90_000, 16 * MS)).toBe(false)
    // The new clock's frames count from here, its headers included.
    for (const t of cadence(32, 400, 16)) f.framed(t)
    expect(f.painted(90_000 + 500, 400 * MS)).toBe(true)
    expect(f.count).toBe(1)
  })

  it('takes a header that comes behind a later one as framed, and ignores one behind the picture', () => {
    const f = createFreezeCounter()
    f.framed(0)
    f.painted(30, 0)
    f.framed(1_000 * MS)
    f.framed(500 * MS) // late, but the host framed it: its pauses were 500 and 500
    expect(f.painted(1_030, 1_000 * MS)).toBe(true)
    f.framed(900 * MS) // behind the picture: says nothing about what comes next
    f.framed(2_000 * MS)
    expect(f.painted(2_030, 2_000 * MS)).toBe(false)
    expect(f.count).toBe(1)
  })

  it('forgets everything at a reset', () => {
    const f = createFreezeCounter()
    onTime(f, cadence(0, 400, 16), 30)
    f.framed(416 * MS)
    f.painted(2_000, 416 * MS)
    expect(f.count).toBe(1)
    f.reset()
    expect(f.count).toBe(0)
    // The first paint after a reset is a first paint.
    f.framed(432 * MS)
    expect(f.painted(9_000, 432 * MS)).toBe(false)
  })
})

describe('FR-135 N11 — a long outage', () => {
  it(`still reads the host's steady framing past ${MAX_FRAMED} headers with no paint`, () => {
    const f = createFreezeCounter()
    f.framed(0)
    f.painted(30, 0)
    // The gate waits for 60 s at 120 fps while the deltas keep arriving.
    const ts = cadence(8, 60_000, 8)
    expect(ts.length).toBeGreaterThan(3 * MAX_FRAMED)
    for (const t of ts) f.framed(t)
    expect(f.painted(60_100, ts.at(-1)!)).toBe(true)
    // And a key frame that paints inside what was folded is judged against it.
    const g = createFreezeCounter()
    g.framed(0)
    g.painted(30, 0)
    for (const t of ts) g.framed(t)
    expect(g.painted(30_100, ts[3_000]!)).toBe(true)
    expect(g.painted(30_101, ts[3_001]!)).toBe(false)
  })

  it('keeps the host’s pause before the folded headers', () => {
    const f = createFreezeCounter()
    f.framed(0)
    f.painted(30, 0)
    // The host paused a second, then framed steadily, and every header after
    // the pause reached the worker before the first of them painted.
    const ts = cadence(1_000, 40_000, 8)
    expect(ts.length).toBeGreaterThan(2 * MAX_FRAMED)
    for (const t of ts) f.framed(t)
    // 100 ms past its latency: the pause explains the rest.
    expect(f.painted(1_000 + 30 + 100, ts[0]!)).toBe(false)
    expect(f.painted(1_000 + 30 + 101, ts[1]!)).toBe(false)
    expect(f.count).toBe(0)
  })
})
