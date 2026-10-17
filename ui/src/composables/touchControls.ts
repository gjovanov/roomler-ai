// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-111 P3 — on-screen controls: rows of buttons, a d-pad and a mouse stick
 * laid over the stage. Each one emits frames every deployed agent already
 * takes: `key` for a key, the mouse frames for a mouse button and for the
 * stick. Pure: the built-in presets, the d-pad's directions, the stick's
 * speed and the driver that turns fingers into presses, with no DOM, so
 * vitest drives them directly.
 */

export type MouseBtn = 'left' | 'right' | 'middle'

/** The HID usages (Keyboard/Keypad page 0x07) the built-in presets bind. A
 *  test checks every one against the stage's own `kbdCodeToHid`. */
export const HID = {
  KeyA: 0x04,
  KeyC: 0x06,
  KeyD: 0x07,
  KeyE: 0x08,
  KeyR: 0x15,
  KeyS: 0x16,
  KeyV: 0x19,
  KeyW: 0x1a,
  KeyZ: 0x1d,
  Enter: 0x28,
  Escape: 0x29,
  Tab: 0x2b,
  Space: 0x2c,
  PageUp: 0x4b,
  PageDown: 0x4e,
  ArrowRight: 0x4f,
  ArrowLeft: 0x50,
  ArrowDown: 0x51,
  ArrowUp: 0x52,
  ControlLeft: 0xe0,
  ShiftLeft: 0xe1,
  AltLeft: 0xe2,
} as const

/** What a button does while it is held: a chord of keys, pressed in order
 *  and released in reverse, or a mouse button. */
export type ControlAction =
  | { kind: 'keys'; keys: number[] }
  | { kind: 'mouse'; btn: MouseBtn }

export interface ButtonControl {
  id: string
  label: string
  action: ControlAction
}

/** Where a control sits: CSS pixels from the overlay's edges. An axis given
 *  no edge is centred. */
export interface ControlPlace {
  left?: number
  right?: number
  top?: number
  bottom?: number
}

export interface RowControl {
  kind: 'row'
  id: string
  at: ControlPlace
  /** `column` stacks the buttons; a row wraps when the stage is narrow. */
  dir?: 'row' | 'column'
  buttons: ButtonControl[]
}

export interface DpadControl {
  kind: 'dpad'
  id: string
  label: string
  at: ControlPlace
  /** Diameter in CSS pixels. */
  size: number
  up: number
  down: number
  left: number
  right: number
}

export interface StickControl {
  kind: 'stick'
  id: string
  label: string
  at: ControlPlace
  size: number
  /** Full deflection moves the pointer this share of the picture's width a
   *  second. */
  speed: number
}

export type TouchControl = RowControl | DpadControl | StickControl

export interface TouchPreset {
  id: string
  name: string
  controls: TouchControl[]
}

const key = (id: string, label: string, ...keys: number[]): ButtonControl => ({
  id,
  label,
  action: { kind: 'keys', keys },
})
const mouse = (id: string, label: string, btn: MouseBtn): ButtonControl => ({
  id,
  label,
  action: { kind: 'mouse', btn },
})

/** The built-ins G1 ships (open decision 2, built to its recommendation): an
 *  office row, arrows, and WASD with mouse look. */
export const BUILTIN_PRESETS: readonly TouchPreset[] = [
  {
    id: 'office',
    name: 'Office keys',
    controls: [
      {
        kind: 'row',
        id: 'office',
        at: { bottom: 12 },
        buttons: [
          key('esc', 'Esc', HID.Escape),
          key('tab', 'Tab', HID.Tab),
          key('alt-tab', 'Alt+Tab', HID.AltLeft, HID.Tab),
          key('copy', 'Ctrl+C', HID.ControlLeft, HID.KeyC),
          key('paste', 'Ctrl+V', HID.ControlLeft, HID.KeyV),
          key('undo', 'Ctrl+Z', HID.ControlLeft, HID.KeyZ),
        ],
      },
    ],
  },
  {
    id: 'arrows',
    name: 'Arrows',
    controls: [
      {
        kind: 'dpad',
        id: 'arrows',
        label: 'Arrow keys',
        at: { left: 16, bottom: 16 },
        size: 150,
        up: HID.ArrowUp,
        down: HID.ArrowDown,
        left: HID.ArrowLeft,
        right: HID.ArrowRight,
      },
      {
        kind: 'row',
        id: 'keys',
        dir: 'column',
        at: { right: 16, bottom: 16 },
        buttons: [
          key('pgup', 'PgUp', HID.PageUp),
          key('pgdn', 'PgDn', HID.PageDown),
          key('space', 'Space', HID.Space),
          key('enter', 'Enter', HID.Enter),
          key('esc', 'Esc', HID.Escape),
        ],
      },
    ],
  },
  {
    id: 'wasd',
    name: 'WASD + mouse look',
    controls: [
      {
        kind: 'dpad',
        id: 'move',
        label: 'W A S D',
        at: { left: 16, bottom: 16 },
        size: 150,
        up: HID.KeyW,
        down: HID.KeyS,
        left: HID.KeyA,
        right: HID.KeyD,
      },
      {
        kind: 'stick',
        id: 'look',
        label: 'Mouse look',
        at: { right: 16, bottom: 16 },
        size: 150,
        speed: 1,
      },
      {
        kind: 'row',
        id: 'act',
        dir: 'column',
        at: { right: 16, bottom: 182 },
        buttons: [
          mouse('fire', 'Fire', 'left'),
          mouse('aim', 'Aim', 'right'),
          key('jump', 'Jump', HID.Space),
        ],
      },
      {
        kind: 'row',
        id: 'keys',
        at: { left: 16, top: 12 },
        buttons: [
          key('esc', 'Esc', HID.Escape),
          key('e', 'E', HID.KeyE),
          key('r', 'R', HID.KeyR),
          key('shift', 'Shift', HID.ShiftLeft),
          key('ctrl', 'Ctrl', HID.ControlLeft),
        ],
      },
    ],
  },
]

export function presetById(id: string | null): TouchPreset | null {
  return BUILTIN_PRESETS.find((p) => p.id === id) ?? null
}

/** The overlay is remembered per device, in this browser: a host that runs a
 *  game keeps its WASD, a work laptop its office row. */
export const CONTROLS_STORAGE_PREFIX = 'roomler-rc-touch-controls:'

/** The preset this browser chose for `deviceId`, or `null` for none (the
 *  default). An id this build does not know, say from a newer one, reads as
 *  none. */
export function readControlsPreset(
  storage: Pick<Storage, 'getItem'> | null | undefined,
  deviceId: string,
): string | null {
  try {
    const v = storage?.getItem(CONTROLS_STORAGE_PREFIX + deviceId) ?? null
    return presetById(v)?.id ?? null
  } catch {
    return null
  }
}

/** The CSS that puts a control at `at`. An axis given no edge is centred. */
export function placeStyle(at: ControlPlace): Record<string, string> {
  const s: Record<string, string> = {}
  const midX = at.left === undefined && at.right === undefined
  const midY = at.top === undefined && at.bottom === undefined
  if (at.left !== undefined) s.left = `${at.left}px`
  if (at.right !== undefined) s.right = `${at.right}px`
  if (at.top !== undefined) s.top = `${at.top}px`
  if (at.bottom !== undefined) s.bottom = `${at.bottom}px`
  if (midX) s.left = '50%'
  if (midY) s.top = '50%'
  if (midX || midY) s.transform = `translate(${midX ? '-50%' : '0'}, ${midY ? '-50%' : '0'})`
  return s
}

/** What a d-pad shows at each arm: the letter of a letter key, an arrow for
 *  an arrow key. */
export function hidLabel(code: number): string {
  if (code >= 0x04 && code <= 0x1d) return String.fromCharCode(65 + code - 0x04)
  switch (code) {
    case HID.ArrowUp:
      return '▲'
    case HID.ArrowDown:
      return '▼'
    case HID.ArrowLeft:
      return '◀'
    case HID.ArrowRight:
      return '▶'
    default:
      return ''
  }
}

/** On a Mac host whose viewer turns Ctrl into Cmd (FR-13), say so on the
 *  buttons, which send what the keyboard would. */
export function controlLabel(label: string, ctrlIsCmd: boolean): string {
  return ctrlIsCmd ? label.replace(/^Ctrl\+/, '⌘') : label
}

/** A finger closer to the d-pad's centre than this share of its radius holds
 *  nothing. */
export const DPAD_DEADZONE = 0.25
/** Each direction covers 135°, centred on its axis, so a diagonal (the 45°
 *  between two of them) holds both keys. */
const DPAD_HALF_ARC = 67.5

/** The keys a finger on the d-pad holds, from its offset from the centre as
 *  a share of the radius (`nx` right, `ny` down). */
export function dpadKeys(c: DpadControl, nx: number, ny: number): number[] {
  if (Math.hypot(nx, ny) < DPAD_DEADZONE) return []
  const a = (Math.atan2(ny, nx) * 180) / Math.PI
  const near = (axis: number) => Math.abs((((a - axis) % 360) + 540) % 360 - 180) < DPAD_HALF_ARC
  const out: number[] = []
  if (near(-90)) out.push(c.up)
  if (near(90)) out.push(c.down)
  if (near(180)) out.push(c.left)
  if (near(0)) out.push(c.right)
  return out
}

/** A deflection under this share of the stick's radius moves nothing. */
export const STICK_DEADZONE = 0.15

/** The stick's motion, in the picture's pixels per second. The speed grows
 *  with the square of the push past the deadzone, so a small push is
 *  precise and a full one crosses `speed` pictures a second. */
export function stickVelocity(
  nx: number,
  ny: number,
  speed: number,
  width: number,
): { vx: number; vy: number } {
  const r = Math.hypot(nx, ny)
  if (r < STICK_DEADZONE) return { vx: 0, vy: 0 }
  const push = (Math.min(r, 1) - STICK_DEADZONE) / (1 - STICK_DEADZONE)
  const v = push * push * speed * width
  return { vx: (nx / r) * v, vy: (ny / r) * v }
}

/** The modifier bits (`mods` on the wire: Ctrl 1, Shift 2, Alt 4, Meta 8)
 *  of a set of held HID usages. */
export function modsOf(codes: Iterable<number>): number {
  let m = 0
  for (const c of codes) {
    if (c === 0xe0 || c === 0xe4) m |= 1
    else if (c === 0xe1 || c === 0xe5) m |= 2
    else if (c === 0xe2 || c === 0xe6) m |= 4
    else if (c === 0xe3 || c === 0xe7) m |= 8
  }
  return m
}

/** Where the controls' presses go: the attached stage turns them into the
 *  frames a keyboard and a mouse send. */
export interface ControlsSink {
  key(code: number, down: boolean): void
  button(btn: MouseBtn, down: boolean): void
  /** Relative pointer motion, in the picture's own pixels. */
  move(dx: number, dy: number): void
  /** A stick starts moving the pointer: an agent without `mouse-rel` picks up
   *  the host pointer's position again. */
  begin(): void
  /** The last stick let go: the next press reads where the host pointer is,
   *  since something else may move it meanwhile. */
  end(): void
  /** The picture's width in its own pixels. */
  width(): number
}

interface Hold {
  keys: number[]
  btn: MouseBtn | null
  dpad: DpadControl | null
  stick: { c: StickControl; nx: number; ny: number } | null
}

/**
 * Turns fingers on the controls into presses. Every finger is tracked by its
 * pointer id, and every key and button is counted, so two fingers holding
 * the same key (a d-pad diagonal and a button) send one press and one
 * release. Sticks move the pointer from `tick`, which the overlay calls
 * every animation frame while one is held.
 */
export function createControlsDriver(sink: ControlsSink) {
  const fingers = new Map<number, Hold>()
  const keyCount = new Map<number, number>()
  const btnCount = new Map<MouseBtn, number>()
  let lastTick: number | null = null

  function keyDown(code: number) {
    const n = keyCount.get(code) ?? 0
    keyCount.set(code, n + 1)
    if (n === 0) sink.key(code, true)
  }
  function keyUp(code: number) {
    const n = keyCount.get(code) ?? 0
    if (n === 0) return
    if (n === 1) {
      keyCount.delete(code)
      sink.key(code, false)
    } else {
      keyCount.set(code, n - 1)
    }
  }
  function btnDown(btn: MouseBtn) {
    const n = btnCount.get(btn) ?? 0
    btnCount.set(btn, n + 1)
    if (n === 0) sink.button(btn, true)
  }
  function btnUp(btn: MouseBtn) {
    const n = btnCount.get(btn) ?? 0
    if (n === 0) return
    if (n === 1) {
      btnCount.delete(btn)
      sink.button(btn, false)
    } else {
      btnCount.set(btn, n - 1)
    }
  }
  function hold(): Hold {
    return { keys: [], btn: null, dpad: null, stick: null }
  }
  /** Move a d-pad finger's keys to `next`: let go of the old first. */
  function setDpadKeys(h: Hold, next: number[]) {
    for (const k of [...h.keys].reverse()) {
      if (!next.includes(k)) keyUp(k)
    }
    for (const k of next) {
      if (!h.keys.includes(k)) keyDown(k)
    }
    h.keys = [...next]
  }
  function release(id: number) {
    const h = fingers.get(id)
    if (!h) return
    fingers.delete(id)
    for (const k of [...h.keys].reverse()) keyUp(k)
    if (h.btn) btnUp(h.btn)
    if (h.stick && ![...fingers.values()].some((f) => f.stick)) sink.end()
  }

  return {
    pressButton(id: number, b: ButtonControl) {
      if (fingers.has(id)) return
      const h = hold()
      if (b.action.kind === 'keys') {
        for (const k of b.action.keys) {
          keyDown(k)
          h.keys.push(k)
        }
      } else {
        btnDown(b.action.btn)
        h.btn = b.action.btn
      }
      fingers.set(id, h)
    },
    pressDpad(id: number, c: DpadControl, nx: number, ny: number) {
      if (fingers.has(id)) return
      const h = hold()
      h.dpad = c
      fingers.set(id, h)
      setDpadKeys(h, dpadKeys(c, nx, ny))
    },
    pressStick(id: number, c: StickControl, nx: number, ny: number, now: number) {
      if (fingers.has(id)) return
      const h = hold()
      h.stick = { c, nx, ny }
      if (![...fingers.values()].some((f) => f.stick)) {
        lastTick = now
        sink.begin()
      }
      fingers.set(id, h)
    },
    /** A held finger moved: `nx`/`ny` are its offset from its control's
     *  centre, as a share of the radius. */
    move(id: number, nx: number, ny: number) {
      const h = fingers.get(id)
      if (!h) return
      if (h.dpad) setDpadKeys(h, dpadKeys(h.dpad, nx, ny))
      else if (h.stick) {
        h.stick.nx = nx
        h.stick.ny = ny
      }
    },
    release,
    /** Let go of everything: the overlay hides, the preset changes, the page
     *  loses the user. */
    releaseAll() {
      for (const id of [...fingers.keys()].reverse()) release(id)
    },
    /** Move the pointer for every held stick up to `now`. Returns whether a
     *  stick is still held, so the overlay keeps calling. */
    tick(now: number): boolean {
      const sticks = [...fingers.values()].filter((f) => f.stick)
      if (sticks.length === 0) {
        lastTick = null
        return false
      }
      const dt = lastTick === null ? 0 : Math.min(Math.max(now - lastTick, 0), 100) / 1000
      lastTick = now
      let dx = 0
      let dy = 0
      const w = sink.width()
      for (const f of sticks) {
        const s = f.stick!
        const v = stickVelocity(s.nx, s.ny, s.c.speed, w)
        dx += v.vx * dt
        dy += v.vy * dt
      }
      if (dx !== 0 || dy !== 0) sink.move(dx, dy)
      return true
    },
    /** The keys a finger holds now (the overlay lights them). */
    keysOf(id: number): number[] {
      return [...(fingers.get(id)?.keys ?? [])]
    },
    active(): boolean {
      return fingers.size > 0
    },
  }
}

export type ControlsDriver = ReturnType<typeof createControlsDriver>
