// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// FR-111 P1 — gestures on the stage in direct mode, as a pure recognizer: touch
// pointer events go in, mouse actions come out. No DOM and no timers, so every
// gesture is a plain test; the wiring (the stage's listeners, the long-press
// timer, the viewer's zoom) lives in useRemoteControl.ts and RemoteControl.vue.
//
// Direct mode keeps "the finger is the mouse" and adds what a touch screen
// needs:
//   one finger   tap: left click where it landed · moved past the slop: a left
//                drag from where it landed · held still: the right button
//   two fingers  a quick tap: the right button · moved together: the wheel ·
//                spread or pinched: the viewer's own zoom (nothing is sent)
// A press is decided only once the finger says what it is, so a long press
// never becomes a left press first, and a second finger never drags with the
// first.

/** One touch pointer at one moment: CSS pixels for the thresholds, the
 *  picture's normalised coordinates (0..1, letterbox-aware) for the frames. */
export interface TouchSample {
  id: number
  px: number
  py: number
  nx: number
  ny: number
  t: number
}

export type GestureAction =
  | { kind: 'press'; btn: 'left' | 'right'; x: number; y: number }
  | { kind: 'move'; x: number; y: number }
  | { kind: 'release'; btn: 'left' | 'right'; x: number; y: number }
  | { kind: 'wheel'; dx: number; dy: number }
  | { kind: 'zoom'; factor: number; cx: number; cy: number }

/** How far a finger may wander and still be a tap or a long press. */
export const SLOP_PX = 8
/** Held still this long, one finger is the right button. */
export const LONG_PRESS_MS = 500
/** Two fingers lifted within this long of the second landing, without
 *  moving, are a two-finger tap. */
export const TWO_FINGER_TAP_MS = 350
/** The distance between two fingers must change this much to be a pinch. */
export const PINCH_SLOP_PX = 24

export interface DirectGestures {
  down(p: TouchSample): GestureAction[]
  move(p: TouchSample): GestureAction[]
  up(p: TouchSample): GestureAction[]
  /** A pointer the browser cancelled: a pending press is dropped, a drag is
   *  released at its last point. */
  cancel(id: number): GestureAction[]
  /** Call once {@link deadline} has passed: the long press, if still due. */
  tick(t: number): GestureAction[]
  /** When {@link tick} next has something to decide, or null. */
  deadline(): number | null
  /** Is any touch pointer down (a gesture under way, or one being waited out)? */
  active(): boolean
}

type Finger = { id: number; start: TouchSample; last: TouchSample }
/** The two-finger part every touch mode shares (FR-111 P1, P2). */
type TwoFinger = { a: Finger; b: Finger; t0: number; mode: 'undecided' | 'scroll' | 'pinch'; cx: number; cy: number; dist: number }
type State =
  | { s: 'idle' }
  | { s: 'pending'; f: Finger }
  | { s: 'dragging'; f: Finger }
  | { s: 'held'; id: number }
  | ({ s: 'two' } & TwoFinger)
  | { s: 'draining' }

const dist = (ax: number, ay: number, bx: number, by: number) => Math.hypot(ax - bx, ay - by)

type WheelOrZoom = Extract<GestureAction, { kind: 'wheel' | 'zoom' }>

function twoFrom(a: Finger, b: Finger, t0: number): TwoFinger {
  return {
    a,
    b,
    t0,
    mode: 'undecided',
    cx: (a.last.px + b.last.px) / 2,
    cy: (a.last.py + b.last.py) / 2,
    dist: dist(a.last.px, a.last.py, b.last.px, b.last.py),
  }
}

/** A move of either finger of a two-finger gesture: decides scroll or pinch
 *  once the fingers say which, then reads the wheel (natural direction: the
 *  content follows the fingers, so fingers moving up turn it down) or the
 *  zoom. Updates `st` in place. */
function twoMove(st: TwoFinger, p: TouchSample): WheelOrZoom[] {
  if (p.id === st.a.id) st.a.last = p
  else st.b.last = p
  const { a, b } = st
  const cx = (a.last.px + b.last.px) / 2
  const cy = (a.last.py + b.last.py) / 2
  const d = dist(a.last.px, a.last.py, b.last.px, b.last.py)
  if (st.mode === 'undecided') {
    const d0 = dist(a.start.px, a.start.py, b.start.px, b.start.py)
    const c0x = (a.start.px + b.start.px) / 2
    const c0y = (a.start.py + b.start.py) / 2
    if (Math.abs(d - d0) > PINCH_SLOP_PX) st.mode = 'pinch'
    else if (dist(cx, cy, c0x, c0y) > SLOP_PX) st.mode = 'scroll'
    else return []
  }
  const out: WheelOrZoom[] = []
  if (st.mode === 'scroll') {
    const dx = st.cx - cx
    const dy = st.cy - cy
    if (dx !== 0 || dy !== 0) out.push({ kind: 'wheel', dx, dy })
  } else if (st.dist > 0 && d > 0) {
    const factor = d / st.dist
    if (factor !== 1) out.push({ kind: 'zoom', factor, cx, cy })
  }
  st.cx = cx
  st.cy = cy
  st.dist = d
  return out
}

/** Was the gesture a two-finger tap (lifted quickly, never moved)? */
function twoTapped(st: TwoFinger, t: number): boolean {
  return st.mode === 'undecided' && t - st.t0 <= TWO_FINGER_TAP_MS
}

/** The agent turns a `mouse_wheel` in pixels into wheel steps at 100 px a step
 *  and rounds EVERY frame on its own (`enigo_backend.rs` `wheel_to_steps`), so
 *  a frame under 50 px scrolls nothing. A finger's scroll arrives in frames of
 *  a few pixels each. */
export const AGENT_WHEEL_STEP_PX = 100

/** #2367 — a mouse or trackpad scroll that pauses this long has ended: its
 *  carry (under one step) is dropped, so it never adds to the next one. */
export const WHEEL_CARRY_IDLE_MS = 400

/** Gathers a gesture's wheel pixels and lets out only whole agent steps,
 *  carrying the rest, so a slow two-finger scroll reaches every deployed agent
 *  instead of rounding to nothing frame by frame. */
export interface WheelSteps {
  /** Add pixels; the frame to send (whole steps), or null while under a step. */
  add(dx: number, dy: number): { dx: number; dy: number } | null
  /** A new gesture starts from nothing. */
  reset(): void
}

export function createWheelSteps(stepPx: number = AGENT_WHEEL_STEP_PX): WheelSteps {
  let ax = 0
  let ay = 0
  return {
    add(dx, dy) {
      ax += dx
      ay += dy
      const sx = Math.trunc(ax / stepPx)
      const sy = Math.trunc(ay / stepPx)
      if (sx === 0 && sy === 0) return null
      ax -= sx * stepPx
      ay -= sy * stepPx
      return { dx: sx * stepPx, dy: sy * stepPx }
    },
    reset() {
      ax = 0
      ay = 0
    },
  }
}

export function createDirectGestures(): DirectGestures {
  let st: State = { s: 'idle' }
  // Every touch pointer currently down, including ones a gesture ignores, so
  // the recognizer knows when the screen is clear again.
  const down = new Set<number>()

  function clearedOrDraining(): State {
    return down.size === 0 ? { s: 'idle' } : { s: 'draining' }
  }

  return {
    down(p) {
      down.add(p.id)
      if (st.s === 'idle') {
        st = { s: 'pending', f: { id: p.id, start: p, last: p } }
        return []
      }
      if (st.s === 'pending') {
        // A second finger before the first decided anything: a two-finger
        // gesture, and the first finger's press never happens.
        st = { s: 'two', ...twoFrom(st.f, { id: p.id, start: p, last: p }, p.t) }
        return []
      }
      // A finger added to a drag, a long press or a two-finger gesture is ignored.
      return []
    },

    move(p) {
      if (st.s === 'pending' && p.id === st.f.id) {
        st.f.last = p
        if (dist(p.px, p.py, st.f.start.px, st.f.start.py) > SLOP_PX) {
          const f = st.f
          st = { s: 'dragging', f }
          return [
            { kind: 'press', btn: 'left', x: f.start.nx, y: f.start.ny },
            { kind: 'move', x: p.nx, y: p.ny },
          ]
        }
        return []
      }
      if (st.s === 'dragging' && p.id === st.f.id) {
        st.f.last = p
        return [{ kind: 'move', x: p.nx, y: p.ny }]
      }
      if (st.s === 'two' && (p.id === st.a.id || p.id === st.b.id)) {
        return twoMove(st, p)
      }
      return []
    },

    up(p) {
      down.delete(p.id)
      if (st.s === 'pending' && p.id === st.f.id) {
        const f = st.f
        st = clearedOrDraining()
        // A tap clicks where the finger landed: a lifting finger jitters.
        return [
          { kind: 'press', btn: 'left', x: f.start.nx, y: f.start.ny },
          { kind: 'release', btn: 'left', x: f.start.nx, y: f.start.ny },
        ]
      }
      if (st.s === 'dragging' && p.id === st.f.id) {
        st = clearedOrDraining()
        return [{ kind: 'release', btn: 'left', x: p.nx, y: p.ny }]
      }
      if (st.s === 'held' && p.id === st.id) {
        st = clearedOrDraining()
        return []
      }
      if (st.s === 'two' && (p.id === st.a.id || p.id === st.b.id)) {
        const two = st
        st = clearedOrDraining()
        if (twoTapped(two, p.t)) {
          const x = (two.a.start.nx + two.b.start.nx) / 2
          const y = (two.a.start.ny + two.b.start.ny) / 2
          return [
            { kind: 'press', btn: 'right', x, y },
            { kind: 'release', btn: 'right', x, y },
          ]
        }
        return []
      }
      if (st.s === 'draining' && down.size === 0) st = { s: 'idle' }
      return []
    },

    cancel(id) {
      down.delete(id)
      if (st.s === 'dragging' && id === st.f.id) {
        const last = st.f.last
        st = clearedOrDraining()
        return [{ kind: 'release', btn: 'left', x: last.nx, y: last.ny }]
      }
      if (
        (st.s === 'pending' && id === st.f.id) ||
        (st.s === 'held' && id === st.id) ||
        (st.s === 'two' && (id === st.a.id || id === st.b.id))
      ) {
        st = clearedOrDraining()
        return []
      }
      if (st.s === 'draining' && down.size === 0) st = { s: 'idle' }
      return []
    },

    tick(t) {
      if (st.s === 'pending' && t >= st.f.start.t + LONG_PRESS_MS) {
        const f = st.f
        st = { s: 'held', id: f.id }
        return [
          { kind: 'press', btn: 'right', x: f.start.nx, y: f.start.ny },
          { kind: 'release', btn: 'right', x: f.start.nx, y: f.start.ny },
        ]
      }
      return []
    },

    deadline() {
      return st.s === 'pending' ? st.f.start.t + LONG_PRESS_MS : null
    },

    active() {
      return down.size > 0
    },
  }
}

// FR-111 P2 — trackpad mode: the screen is a laptop's trackpad. The finger no
// longer IS the pointer; it moves the host's pointer, which stays where it is
// between touches.
//   one finger   moved: the pointer moves by the finger's travel · a tap: a
//                left click where the pointer is · held 500 ms, then moved: a
//                left drag, let go when the finger lifts
//   two fingers  as in direct mode: a quick tap is the right button, moving
//                together is the wheel, spreading or pinching is the zoom
// The wiring turns `pointer` travel into FR-109's `mouse_rel` for an agent that
// advertises `mouse-rel`, and into absolute moves from the host pointer's last
// known position for one that does not.

export type TrackpadAction =
  | { kind: 'pointer'; dx: number; dy: number }
  | { kind: 'press'; btn: 'left' | 'right' }
  | { kind: 'release'; btn: 'left' | 'right' }
  | WheelOrZoom

export interface TrackpadGestures {
  down(p: TouchSample): TrackpadAction[]
  move(p: TouchSample): TrackpadAction[]
  up(p: TouchSample): TrackpadAction[]
  /** A cancelled finger: a held drag is let go; nothing else is sent. */
  cancel(id: number): TrackpadAction[]
  /** Call once {@link deadline} has passed: the hold that arms a drag. */
  tick(t: number): TrackpadAction[]
  deadline(): number | null
  active(): boolean
}

type PadState =
  | { s: 'idle' }
  | { s: 'pending'; f: Finger }
  | { s: 'moving'; f: Finger }
  | { s: 'armed'; f: Finger }
  | ({ s: 'two' } & TwoFinger)
  | { s: 'draining' }

export function createTrackpadGestures(): TrackpadGestures {
  let st: PadState = { s: 'idle' }
  const down = new Set<number>()
  const settle = (): PadState => (down.size === 0 ? { s: 'idle' } : { s: 'draining' })

  return {
    down(p) {
      down.add(p.id)
      if (st.s === 'idle') {
        st = { s: 'pending', f: { id: p.id, start: p, last: p } }
      } else if (st.s === 'pending') {
        st = { s: 'two', ...twoFrom(st.f, { id: p.id, start: p, last: p }, p.t) }
      }
      // A finger added while the pointer moves or a drag is held is ignored.
      return []
    },

    move(p) {
      if (st.s === 'pending' && p.id === st.f.id) {
        if (dist(p.px, p.py, st.f.start.px, st.f.start.py) <= SLOP_PX) {
          st.f.last = p
          return []
        }
        // Past the slop: the pointer moves, by everything the finger has
        // travelled, so the slop is not swallowed.
        const f = st.f
        f.last = p
        st = { s: 'moving', f }
        return [{ kind: 'pointer', dx: p.px - f.start.px, dy: p.py - f.start.py }]
      }
      if ((st.s === 'moving' || st.s === 'armed') && p.id === st.f.id) {
        const dx = p.px - st.f.last.px
        const dy = p.py - st.f.last.py
        st.f.last = p
        return dx !== 0 || dy !== 0 ? [{ kind: 'pointer', dx, dy }] : []
      }
      if (st.s === 'two' && (p.id === st.a.id || p.id === st.b.id)) {
        return twoMove(st, p)
      }
      return []
    },

    up(p) {
      down.delete(p.id)
      if (st.s === 'pending' && p.id === st.f.id) {
        st = settle()
        return [
          { kind: 'press', btn: 'left' },
          { kind: 'release', btn: 'left' },
        ]
      }
      if (st.s === 'moving' && p.id === st.f.id) {
        st = settle()
        return []
      }
      if (st.s === 'armed' && p.id === st.f.id) {
        st = settle()
        return [{ kind: 'release', btn: 'left' }]
      }
      if (st.s === 'two' && (p.id === st.a.id || p.id === st.b.id)) {
        const two = st
        st = settle()
        return twoTapped(two, p.t)
          ? [
              { kind: 'press', btn: 'right' },
              { kind: 'release', btn: 'right' },
            ]
          : []
      }
      if (st.s === 'draining' && down.size === 0) st = { s: 'idle' }
      return []
    },

    cancel(id) {
      down.delete(id)
      if (st.s === 'armed' && id === st.f.id) {
        st = settle()
        return [{ kind: 'release', btn: 'left' }]
      }
      if (
        ((st.s === 'pending' || st.s === 'moving') && id === st.f.id) ||
        (st.s === 'two' && (id === st.a.id || id === st.b.id))
      ) {
        st = settle()
        return []
      }
      if (st.s === 'draining' && down.size === 0) st = { s: 'idle' }
      return []
    },

    tick(t) {
      if (st.s === 'pending' && t >= st.f.start.t + LONG_PRESS_MS) {
        st = { s: 'armed', f: st.f }
        return [{ kind: 'press', btn: 'left' }]
      }
      return []
    },

    deadline() {
      return st.s === 'pending' ? st.f.start.t + LONG_PRESS_MS : null
    },

    active() {
      return down.size > 0
    },
  }
}
