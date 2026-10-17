// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import {
  BUILTIN_PRESETS,
  CONTROLS_STORAGE_PREFIX,
  controlLabel,
  createControlsDriver,
  dpadKeys,
  HID,
  hidLabel,
  modsOf,
  placeStyle,
  readControlsPreset,
  stickVelocity,
  type ButtonControl,
  type ControlsSink,
  type DpadControl,
  type MouseBtn,
  type StickControl,
} from '@/composables/touchControls'
import { kbdCodeToHid } from '@/composables/useRemoteControl'

type Sent =
  | { k: number; down: boolean }
  | { b: MouseBtn; down: boolean }
  | { dx: number; dy: number }
  | { begin: true }
  | { end: true }

function recordingSink(width = 1280): { sink: ControlsSink; sent: Sent[] } {
  const sent: Sent[] = []
  return {
    sent,
    sink: {
      key: (k, down) => sent.push({ k, down }),
      button: (b, down) => sent.push({ b, down }),
      move: (dx, dy) => sent.push({ dx, dy }),
      begin: () => sent.push({ begin: true }),
      end: () => sent.push({ end: true }),
      width: () => width,
    },
  }
}

const wasd: DpadControl = {
  kind: 'dpad',
  id: 'move',
  label: 'W A S D',
  at: {},
  size: 150,
  up: HID.KeyW,
  down: HID.KeyS,
  left: HID.KeyA,
  right: HID.KeyD,
}
const look: StickControl = { kind: 'stick', id: 'look', label: 'look', at: {}, size: 150, speed: 1 }
const copy: ButtonControl = { id: 'copy', label: 'Ctrl+C', action: { kind: 'keys', keys: [HID.ControlLeft, HID.KeyC] } }
const ctrl: ButtonControl = { id: 'ctrl', label: 'Ctrl', action: { kind: 'keys', keys: [HID.ControlLeft] } }
const fire: ButtonControl = { id: 'fire', label: 'Fire', action: { kind: 'mouse', btn: 'left' } }

describe('FR-111 P3 presets', () => {
  it('names every key by the HID usage the keyboard path sends for it', () => {
    for (const [name, code] of Object.entries(HID)) {
      expect(kbdCodeToHid(name), name).toBe(code)
    }
  })

  it('ships the office row, arrows and WASD with mouse look, each control and button with its own id', () => {
    expect(BUILTIN_PRESETS.map((p) => p.id)).toEqual(['office', 'arrows', 'wasd'])
    const codes = new Set<number>(Object.values(HID))
    for (const p of BUILTIN_PRESETS) {
      expect(p.name.length).toBeGreaterThan(0)
      const ids = p.controls.map((c) => c.id)
      expect(new Set(ids).size, p.id).toBe(ids.length)
      for (const c of p.controls) {
        if (c.kind !== 'row') continue
        const bids = c.buttons.map((b) => b.id)
        expect(new Set(bids).size, `${p.id}/${c.id}`).toBe(bids.length)
        for (const b of c.buttons) {
          if (b.action.kind === 'keys') {
            expect(b.action.keys.length).toBeGreaterThan(0)
            for (const k of b.action.keys) expect(codes.has(k), `${p.id}/${b.id}`).toBe(true)
          }
        }
      }
    }
  })
})

describe('FR-111 P3 readControlsPreset', () => {
  const store = (m: Record<string, string>): Pick<Storage, 'getItem'> => ({
    getItem: (k: string) => m[k] ?? null,
  })

  it('is off unless this browser chose a preset for this device', () => {
    expect(readControlsPreset(store({}), 'dev1')).toBeNull()
    expect(readControlsPreset(null, 'dev1')).toBeNull()
    const s = store({ [CONTROLS_STORAGE_PREFIX + 'dev1']: 'wasd' })
    expect(readControlsPreset(s, 'dev1')).toBe('wasd')
    expect(readControlsPreset(s, 'dev2')).toBeNull()
  })

  it('reads an id this build does not know, or storage that throws, as off', () => {
    expect(readControlsPreset(store({ [CONTROLS_STORAGE_PREFIX + 'dev1']: 'racing' }), 'dev1')).toBeNull()
    expect(readControlsPreset({ getItem: () => { throw new Error('SecurityError') } }, 'dev1')).toBeNull()
  })
})

describe('FR-111 P3 placeStyle and hidLabel', () => {
  it('places a control by its edges and centres an axis with none', () => {
    expect(placeStyle({ left: 16, bottom: 16 })).toEqual({ left: '16px', bottom: '16px' })
    expect(placeStyle({ bottom: 12 })).toEqual({ bottom: '12px', left: '50%', transform: 'translate(-50%, 0)' })
    expect(placeStyle({})).toEqual({ left: '50%', top: '50%', transform: 'translate(-50%, -50%)' })
  })

  it('labels a d-pad arm with its letter or its arrow', () => {
    expect([HID.KeyW, HID.KeyA, HID.KeyS, HID.KeyD].map(hidLabel)).toEqual(['W', 'A', 'S', 'D'])
    expect([HID.ArrowUp, HID.ArrowDown, HID.ArrowLeft, HID.ArrowRight].map(hidLabel)).toEqual(['▲', '▼', '◀', '▶'])
    expect(hidLabel(HID.Space)).toBe('')
  })
})

describe('FR-111 P3 controlLabel', () => {
  it('shows Cmd for a Ctrl chord on a Mac host that turns Ctrl into Cmd', () => {
    expect(controlLabel('Ctrl+C', true)).toBe('⌘C')
    expect(controlLabel('Ctrl+C', false)).toBe('Ctrl+C')
    expect(controlLabel('Alt+Tab', true)).toBe('Alt+Tab')
    expect(controlLabel('Ctrl', true)).toBe('Ctrl')
  })
})

describe('FR-111 P3 dpadKeys', () => {
  it('holds nothing near the centre', () => {
    expect(dpadKeys(wasd, 0, 0)).toEqual([])
    expect(dpadKeys(wasd, 0.2, 0)).toEqual([])
  })

  it('holds one key along an axis and two on a diagonal', () => {
    expect(dpadKeys(wasd, 0, -1)).toEqual([HID.KeyW])
    expect(dpadKeys(wasd, 0, 1)).toEqual([HID.KeyS])
    expect(dpadKeys(wasd, -1, 0)).toEqual([HID.KeyA])
    expect(dpadKeys(wasd, 1, 0)).toEqual([HID.KeyD])
    // 10° below right is still right; 45° is the diagonal.
    const deg = (d: number) => [Math.cos((d * Math.PI) / 180), Math.sin((d * Math.PI) / 180)] as const
    expect(dpadKeys(wasd, ...deg(10))).toEqual([HID.KeyD])
    expect(dpadKeys(wasd, ...deg(45))).toEqual([HID.KeyS, HID.KeyD])
    expect(dpadKeys(wasd, ...deg(-135))).toEqual([HID.KeyW, HID.KeyA])
  })
})

describe('FR-111 P3 stickVelocity', () => {
  it('moves nothing inside the deadzone and a picture a second at full push', () => {
    expect(stickVelocity(0.1, 0, 1, 1280)).toEqual({ vx: 0, vy: 0 })
    const full = stickVelocity(1, 0, 1, 1280)
    expect(full.vx).toBeCloseTo(1280)
    expect(full.vy).toBeCloseTo(0)
    // Past the rim it stays at full speed.
    expect(stickVelocity(0, -2, 1, 1280).vy).toBeCloseTo(-1280)
  })

  it('grows with the square of the push, so a small push is precise', () => {
    const half = stickVelocity(0.575, 0, 1, 1000) // halfway between the deadzone and the rim
    expect(half.vx).toBeCloseTo(250)
  })
})

describe('FR-111 P3 modsOf', () => {
  it('reads the modifier bits from the held usages', () => {
    expect(modsOf([])).toBe(0)
    expect(modsOf([0xe0, 0x06])).toBe(1)
    expect(modsOf([0xe1, 0xe6])).toBe(2 | 4)
    expect(modsOf([0xe3])).toBe(8)
  })
})

describe('FR-111 P3 createControlsDriver', () => {
  it('a chord presses in order and releases in reverse', () => {
    const { sink, sent } = recordingSink()
    const d = createControlsDriver(sink)
    d.pressButton(1, copy)
    d.release(1)
    expect(sent).toEqual([
      { k: HID.ControlLeft, down: true },
      { k: HID.KeyC, down: true },
      { k: HID.KeyC, down: false },
      { k: HID.ControlLeft, down: false },
    ])
    expect(d.active()).toBe(false)
  })

  it('two fingers holding one key press it once and release it after the last', () => {
    const { sink, sent } = recordingSink()
    const d = createControlsDriver(sink)
    d.pressButton(1, ctrl)
    d.pressButton(2, copy)
    d.release(1)
    expect(sent).toEqual([
      { k: HID.ControlLeft, down: true },
      { k: HID.KeyC, down: true },
    ])
    d.release(2)
    expect(sent.slice(2)).toEqual([
      { k: HID.KeyC, down: false },
      { k: HID.ControlLeft, down: false },
    ])
  })

  it('a finger sliding round the d-pad lets go of the old key before it presses the new one', () => {
    const { sink, sent } = recordingSink()
    const d = createControlsDriver(sink)
    d.pressDpad(1, wasd, 0, -1) // up
    d.move(1, 0.7, -0.7) // up-right
    expect(d.keysOf(1)).toEqual([HID.KeyW, HID.KeyD])
    d.move(1, 1, 0) // right
    d.move(1, 0, 0) // back to the centre
    d.release(1)
    expect(sent).toEqual([
      { k: HID.KeyW, down: true },
      { k: HID.KeyD, down: true },
      { k: HID.KeyW, down: false },
      { k: HID.KeyD, down: false },
    ])
  })

  it('a mouse button holds the button', () => {
    const { sink, sent } = recordingSink()
    const d = createControlsDriver(sink)
    d.pressButton(3, fire)
    d.release(3)
    expect(sent).toEqual([
      { b: 'left', down: true },
      { b: 'left', down: false },
    ])
  })

  it('a held stick moves the pointer every tick, and stops when it is let go', () => {
    const { sink, sent } = recordingSink(1000)
    const d = createControlsDriver(sink)
    d.pressStick(4, look, 1, 0, 1000)
    expect(sent).toEqual([{ begin: true }])
    expect(d.tick(1100)).toBe(true) // 100 ms at full push: a tenth of the picture
    const m = sent[1] as { dx: number; dy: number }
    expect(m.dx).toBeCloseTo(100)
    expect(m.dy).toBeCloseTo(0)
    d.move(4, 0.05, 0) // inside the deadzone
    expect(d.tick(1200)).toBe(true)
    expect(sent.length).toBe(2)
    d.release(4)
    expect(sent[2]).toEqual({ end: true })
    expect(d.tick(1300)).toBe(false)
    expect(sent.length).toBe(3)
  })

  it('two sticks begin once and end only when the last one lets go', () => {
    const { sink, sent } = recordingSink(1000)
    const d = createControlsDriver(sink)
    d.pressStick(4, look, 0, 0, 0)
    d.pressStick(5, look, 0, 0, 0)
    d.release(4)
    expect(sent).toEqual([{ begin: true }])
    d.release(5)
    expect(sent).toEqual([{ begin: true }, { end: true }])
  })

  it('a stall between ticks never throws the pointer: one tick moves at most 100 ms', () => {
    const { sink, sent } = recordingSink(1000)
    const d = createControlsDriver(sink)
    d.pressStick(4, look, 1, 0, 0)
    d.tick(5000)
    expect((sent[1] as { dx: number }).dx).toBeCloseTo(100)
  })

  it('releaseAll lets go of every finger, and ignores a second press by the same finger', () => {
    const { sink, sent } = recordingSink()
    const d = createControlsDriver(sink)
    d.pressButton(1, copy)
    d.pressButton(1, fire) // the same finger: ignored
    d.pressDpad(2, wasd, -1, 0)
    d.move(9, 1, 0) // a finger the driver never saw: ignored
    d.releaseAll()
    expect(sent).toEqual([
      { k: HID.ControlLeft, down: true },
      { k: HID.KeyC, down: true },
      { k: HID.KeyA, down: true },
      { k: HID.KeyA, down: false },
      { k: HID.KeyC, down: false },
      { k: HID.ControlLeft, down: false },
    ])
    expect(d.active()).toBe(false)
  })
})
