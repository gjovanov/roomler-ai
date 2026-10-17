// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import {
  AGENT_WHEEL_STEP_PX,
  createDirectGestures,
  createTrackpadGestures,
  createWheelSteps,
  LONG_PRESS_MS,
  SLOP_PX,
  TWO_FINGER_TAP_MS,
  type TouchSample,
} from '@/composables/touchGestures'

/** A sample on a 1000×500 CSS-pixel picture: normalised = px / size. */
function s(id: number, px: number, py: number, t: number): TouchSample {
  return { id, px, py, nx: px / 1000, ny: py / 500, t }
}

describe('FR-111 direct gestures: one finger', () => {
  it('a tap is a left click where the finger landed', () => {
    const g = createDirectGestures()
    expect(g.down(s(1, 100, 100, 0))).toEqual([])
    // A lifting finger jitters inside the slop; the click stays where it landed.
    expect(g.move(s(1, 103, 102, 50))).toEqual([])
    expect(g.up(s(1, 104, 103, 90))).toEqual([
      { kind: 'press', btn: 'left', x: 0.1, y: 0.2 },
      { kind: 'release', btn: 'left', x: 0.1, y: 0.2 },
    ])
  })

  it('moving past the slop starts a left drag from where the finger landed', () => {
    const g = createDirectGestures()
    g.down(s(1, 100, 100, 0))
    expect(g.move(s(1, 100 + SLOP_PX + 1, 100, 40))).toEqual([
      { kind: 'press', btn: 'left', x: 0.1, y: 0.2 },
      { kind: 'move', x: (100 + SLOP_PX + 1) / 1000, y: 0.2 },
    ])
    expect(g.move(s(1, 200, 150, 80))).toEqual([{ kind: 'move', x: 0.2, y: 0.3 }])
    expect(g.up(s(1, 210, 160, 120))).toEqual([{ kind: 'release', btn: 'left', x: 0.21, y: 0.32 }])
  })

  it('held still, it is the right button, and never a left press first', () => {
    const g = createDirectGestures()
    g.down(s(1, 300, 250, 1000))
    expect(g.deadline()).toBe(1000 + LONG_PRESS_MS)
    expect(g.tick(1000 + LONG_PRESS_MS - 1)).toEqual([])
    expect(g.tick(1000 + LONG_PRESS_MS)).toEqual([
      { kind: 'press', btn: 'right', x: 0.3, y: 0.5 },
      { kind: 'release', btn: 'right', x: 0.3, y: 0.5 },
    ])
    expect(g.deadline()).toBeNull()
    // After the long press the finger's moves and its lift send nothing.
    expect(g.move(s(1, 400, 300, 1600))).toEqual([])
    expect(g.up(s(1, 400, 300, 1700))).toEqual([])
  })

  it('a drag never turns into a long press', () => {
    const g = createDirectGestures()
    g.down(s(1, 100, 100, 0))
    g.move(s(1, 120, 100, 50))
    expect(g.deadline()).toBeNull()
    expect(g.tick(LONG_PRESS_MS * 2)).toEqual([])
  })

  it('a cancelled pending press sends nothing; a cancelled drag lets go at its last point', () => {
    const g = createDirectGestures()
    g.down(s(1, 100, 100, 0))
    expect(g.cancel(1)).toEqual([])
    g.down(s(2, 100, 100, 10))
    g.move(s(2, 150, 100, 40))
    expect(g.cancel(2)).toEqual([{ kind: 'release', btn: 'left', x: 0.15, y: 0.2 }])
  })

  it('a second finger added to a drag is ignored, and the drag still ends with its release', () => {
    const g = createDirectGestures()
    g.down(s(1, 100, 100, 0))
    g.move(s(1, 130, 100, 30))
    expect(g.down(s(2, 500, 300, 60))).toEqual([])
    expect(g.move(s(2, 520, 300, 70))).toEqual([])
    expect(g.up(s(1, 140, 100, 90))).toEqual([{ kind: 'release', btn: 'left', x: 0.14, y: 0.2 }])
    // The ignored finger lifts; the screen is clear and a new tap works.
    expect(g.up(s(2, 520, 300, 100))).toEqual([])
    g.down(s(3, 100, 100, 200))
    expect(g.up(s(3, 100, 100, 260))).toHaveLength(2)
  })
})

describe('FR-111 direct gestures: two fingers', () => {
  it('a quick two-finger tap is the right button, between the fingers', () => {
    const g = createDirectGestures()
    g.down(s(1, 100, 100, 0))
    g.down(s(2, 300, 100, 40))
    expect(g.up(s(1, 101, 100, 150))).toEqual([
      { kind: 'press', btn: 'right', x: 0.2, y: 0.2 },
      { kind: 'release', btn: 'right', x: 0.2, y: 0.2 },
    ])
    // The second finger's lift sends nothing more.
    expect(g.up(s(2, 300, 100, 170))).toEqual([])
  })

  it('two fingers lifted slowly without moving are not a tap', () => {
    const g = createDirectGestures()
    g.down(s(1, 100, 100, 0))
    g.down(s(2, 300, 100, 20))
    expect(g.up(s(1, 100, 100, 20 + TWO_FINGER_TAP_MS + 1))).toEqual([])
  })

  it('moving together turns the wheel, the content following the fingers', () => {
    const g = createDirectGestures()
    g.down(s(1, 100, 300, 0))
    g.down(s(2, 300, 300, 10))
    // Both move up 20 px: past the slop, so it is a scroll; the wheel turns down.
    g.move(s(1, 100, 280, 30))
    expect(g.move(s(2, 300, 280, 40))).toEqual([{ kind: 'wheel', dx: 0, dy: 10 }])
    expect(g.move(s(1, 100, 260, 50))).toEqual([{ kind: 'wheel', dx: 0, dy: 10 }])
    // A scroll ends without a click, and never presses a button.
    expect(g.up(s(1, 100, 260, 60))).toEqual([])
    expect(g.up(s(2, 300, 280, 70))).toEqual([])
  })

  it('spreading the fingers zooms the viewer and sends nothing to the host', () => {
    const g = createDirectGestures()
    g.down(s(1, 400, 250, 0))
    g.down(s(2, 600, 250, 10))
    // From 200 px apart to 260: past the pinch slop.
    const out = g.move(s(2, 660, 250, 40))
    expect(out).toHaveLength(1)
    expect(out[0]).toMatchObject({ kind: 'zoom', cx: 530, cy: 250 })
    expect((out[0] as { factor: number }).factor).toBeCloseTo(260 / 200, 6)
    const out2 = g.move(s(1, 340, 250, 60))
    expect((out2[0] as { factor: number }).factor).toBeCloseTo(320 / 260, 6)
    expect(g.up(s(1, 340, 250, 80))).toEqual([])
  })

  it('after a two-finger gesture, a finger still down blocks nothing once it lifts', () => {
    const g = createDirectGestures()
    g.down(s(1, 100, 100, 0))
    g.down(s(2, 300, 100, 10))
    g.up(s(1, 100, 100, 500))
    // The remaining finger and a newcomer are ignored until the screen is clear.
    expect(g.down(s(3, 500, 300, 520))).toEqual([])
    expect(g.up(s(3, 500, 300, 560))).toEqual([])
    expect(g.up(s(2, 300, 100, 600))).toEqual([])
    g.down(s(4, 100, 100, 700))
    expect(g.up(s(4, 100, 100, 750))).toEqual([
      { kind: 'press', btn: 'left', x: 0.1, y: 0.2 },
      { kind: 'release', btn: 'left', x: 0.1, y: 0.2 },
    ])
  })
})

describe('FR-111 createWheelSteps', () => {
  it('a slow scroll reaches the agent as whole steps instead of rounding away frame by frame', () => {
    // The field's two-finger scroll: one frame of 12.3 px, then twenty of 6.16 px (135.5 px in all).
    const w = createWheelSteps()
    const sent: Array<{ dx: number; dy: number }> = []
    for (const dy of [12.3125, ...Array.from({ length: 20 }, () => 6.15625)]) {
      const step = w.add(0, dy)
      if (step) sent.push(step)
    }
    // Each frame alone is under half a step, which the agent rounds to nothing; together they are one.
    expect(sent).toEqual([{ dx: 0, dy: AGENT_WHEEL_STEP_PX }])
  })

  it('carries the remainder, both ways, and lets out several steps at once', () => {
    const w = createWheelSteps()
    expect(w.add(0, 90)).toBeNull()
    expect(w.add(0, 20)).toEqual({ dx: 0, dy: 100 })
    expect(w.add(0, -30)).toBeNull() // carried 10, now -20
    expect(w.add(-250, -190)).toEqual({ dx: -200, dy: -200 })
  })

  it('starts a new gesture from nothing', () => {
    const w = createWheelSteps()
    w.add(0, 90)
    w.reset()
    expect(w.add(0, 20)).toBeNull()
  })
})

describe('FR-111 trackpad gestures', () => {
  it('a moving finger moves the pointer by everything it travelled, the slop included', () => {
    const g = createTrackpadGestures()
    g.down(s(1, 100, 100, 0))
    expect(g.move(s(1, 105, 100, 20))).toEqual([])
    expect(g.move(s(1, 112, 104, 40))).toEqual([{ kind: 'pointer', dx: 12, dy: 4 }])
    expect(g.move(s(1, 120, 100, 60))).toEqual([{ kind: 'pointer', dx: 8, dy: -4 }])
    // Lifting a moved finger clicks nothing: the pointer just stays.
    expect(g.up(s(1, 120, 100, 80))).toEqual([])
  })

  it('a tap clicks where the pointer is, carrying no position', () => {
    const g = createTrackpadGestures()
    g.down(s(1, 300, 200, 0))
    expect(g.up(s(1, 302, 201, 90))).toEqual([
      { kind: 'press', btn: 'left' },
      { kind: 'release', btn: 'left' },
    ])
  })

  it('held 500 ms, then moved, it drags with the left button, let go when the finger lifts', () => {
    const g = createTrackpadGestures()
    g.down(s(1, 300, 200, 1000))
    expect(g.deadline()).toBe(1000 + LONG_PRESS_MS)
    expect(g.tick(1000 + LONG_PRESS_MS)).toEqual([{ kind: 'press', btn: 'left' }])
    expect(g.move(s(1, 340, 210, 1600))).toEqual([{ kind: 'pointer', dx: 40, dy: 10 }])
    expect(g.up(s(1, 340, 210, 1700))).toEqual([{ kind: 'release', btn: 'left' }])
  })

  it('a cancelled held drag is let go; a cancelled move sends nothing', () => {
    const g = createTrackpadGestures()
    g.down(s(1, 300, 200, 0))
    g.tick(LONG_PRESS_MS)
    expect(g.cancel(1)).toEqual([{ kind: 'release', btn: 'left' }])
    g.down(s(2, 300, 200, 1000))
    g.move(s(2, 330, 200, 1020))
    expect(g.cancel(2)).toEqual([])
  })

  it('two fingers behave as in direct mode, the right button carrying no position', () => {
    const g = createTrackpadGestures()
    g.down(s(1, 100, 100, 0))
    g.down(s(2, 300, 100, 30))
    expect(g.up(s(1, 100, 100, 120))).toEqual([
      { kind: 'press', btn: 'right' },
      { kind: 'release', btn: 'right' },
    ])
    expect(g.up(s(2, 300, 100, 140))).toEqual([])
    g.down(s(3, 100, 300, 500))
    g.down(s(4, 300, 300, 510))
    g.move(s(3, 100, 280, 530))
    expect(g.move(s(4, 300, 280, 540))).toEqual([{ kind: 'wheel', dx: 0, dy: 10 }])
  })
})
