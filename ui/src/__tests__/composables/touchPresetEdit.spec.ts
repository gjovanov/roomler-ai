// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import { BUILTIN_PRESETS, HID, type TouchControl, type TouchPreset } from '@/composables/touchControls'
import {
  copyForEdit,
  describeRefusal,
  freeId,
  LIMITS,
  MINE_PRESET_PREFIX,
  MY_PRESETS_STORAGE_PREFIX,
  newButton,
  newDpad,
  newMineId,
  newRow,
  newStick,
  readMyPresets,
  touchKeyOk,
  validatePreset,
  writeMyPresets,
} from '@/composables/touchPresetEdit'

/** A layout with every kind of control and action, valid as it is. */
function full(): { name: string; controls: TouchControl[] } {
  return {
    name: 'Everything',
    controls: [
      {
        kind: 'row',
        id: 'keys',
        at: { bottom: 12 },
        buttons: [
          { id: 'esc', label: 'Esc', action: { kind: 'keys', keys: [HID.Escape] } },
          { id: 'click', label: 'Click', action: { kind: 'mouse', btn: 'left' } },
          {
            id: 'paste',
            label: 'Paste x2',
            action: {
              kind: 'macro',
              steps: [
                { op: 'press', key: HID.ControlLeft },
                { op: 'tap', key: HID.KeyV, wait_ms: 120 },
                { op: 'release', key: HID.ControlLeft },
                { op: 'tap', btn: 'right' },
              ],
            },
          },
        ],
      },
      { kind: 'dpad', id: 'pad', label: 'Move', at: { left: 16, bottom: 16 }, size: 150, up: HID.KeyW, down: HID.KeyS, left: HID.KeyA, right: HID.KeyD },
      { kind: 'stick', id: 'look', label: 'Look', at: { right: 16, bottom: 16 }, size: 150, speed: 1 },
    ],
  }
}

describe('FR-111 P4b-2 validatePreset (the server rules, here)', () => {
  it('passes a full layout and every built-in, so a copied built-in can always be published', () => {
    expect(validatePreset(full())).toBeNull()
    for (const p of BUILTIN_PRESETS) expect(validatePreset(p), p.id).toBeNull()
  })

  it('refuses what the server refuses, with the server code', () => {
    type Edit = (p: ReturnType<typeof full>) => void
    const row = (p: ReturnType<typeof full>) => p.controls[0] as Extract<TouchControl, { kind: 'row' }>
    const cases: Array<[string, Edit]> = [
      ['name_empty', (p) => (p.name = '   ')],
      ['name_invalid', (p) => (p.name = 'n'.repeat(49))],
      ['name_invalid', (p) => (p.name = 'two\nlines')],
      ['no_controls', (p) => (p.controls = [])],
      ['too_many_controls', (p) => (p.controls = Array.from({ length: 25 }, (_, i) => ({ ...p.controls[2], id: `s${i}` }) as TouchControl))],
      ['id_duplicate', (p) => (p.controls[1].id = 'keys')],
      ['id_duplicate', (p) => (row(p).buttons[1].id = 'esc')],
      ['id_invalid', (p) => (p.controls[1].id = '')],
      ['label_invalid', (p) => ((p.controls[1] as { label: string }).label = 'a label far too long')],
      ['label_invalid', (p) => (row(p).buttons[0].label = ' ')],
      ['row_buttons', (p) => (row(p).buttons = [])],
      ['chord_keys', (p) => (row(p).buttons[0].action = { kind: 'keys', keys: [] })],
      ['chord_keys', (p) => (row(p).buttons[0].action = { kind: 'keys', keys: [4, 5, 6, 7, 8] })],
      ['key_invalid', (p) => (row(p).buttons[0].action = { kind: 'keys', keys: [0xa5] })],
      ['key_invalid', (p) => ((p.controls[1] as { up: number }).up = 0xf0)],
      ['size_invalid', (p) => ((p.controls[1] as { size: number }).size = 47)],
      ['size_invalid', (p) => ((p.controls[2] as { size: number }).size = 401)],
      ['speed_invalid', (p) => ((p.controls[2] as { speed: number }).speed = 0)],
      ['speed_invalid', (p) => ((p.controls[2] as { speed: number }).speed = 5.5)],
      ['place_both_edges', (p) => (p.controls[1].at = { left: 1, right: 1 })],
      ['place_invalid', (p) => (p.controls[1].at = { left: -1 })],
      ['place_invalid', (p) => (p.controls[1].at = { top: 4001 })],
      ['macro_steps', (p) => (row(p).buttons[2].action = { kind: 'macro', steps: [] })],
      ['macro_steps', (p) => (row(p).buttons[2].action = { kind: 'macro', steps: Array(33).fill({ op: 'tap', key: 4 }) })],
      ['step_key_or_button', (p) => (row(p).buttons[2].action = { kind: 'macro', steps: [{ op: 'tap', key: 4, btn: 'left' }] })],
      ['step_key_or_button', (p) => (row(p).buttons[2].action = { kind: 'macro', steps: [{ op: 'tap' }] })],
      ['key_invalid', (p) => (row(p).buttons[2].action = { kind: 'macro', steps: [{ op: 'tap', key: 0xe9 }] })],
      ['step_wait', (p) => (row(p).buttons[2].action = { kind: 'macro', steps: [{ op: 'tap', key: 4, wait_ms: 5001 }] })],
      ['macro_too_long', (p) => (row(p).buttons[2].action = { kind: 'macro', steps: Array(7).fill({ op: 'tap', key: 4, wait_ms: 5000 }) })],
    ]
    for (const [code, edit] of cases) {
      const p = full()
      edit(p)
      expect(validatePreset(p), code).toBe(code)
      expect(describeRefusal(code)).not.toBe(describeRefusal('something_new'))
    }
  })

  it('reads the keyboard page only', () => {
    expect([0x04, 0x73, 0xa4, 0xe0, 0xe7].every(touchKeyOk)).toBe(true)
    expect([0x00, 0x03, 0xa5, 0xdf, 0xe8, 1.5, '4', undefined].some(touchKeyOk)).toBe(false)
  })
})

describe('FR-111 P4b-2 a member’s own presets in this browser', () => {
  function memStorage() {
    const m = new Map<string, string>()
    return {
      m,
      getItem: (k: string) => m.get(k) ?? null,
      setItem: (k: string, v: string) => void m.set(k, v),
    }
  }

  it('round-trips through storage, per user', () => {
    const s = memStorage()
    const mine: TouchPreset = { id: `${MINE_PRESET_PREFIX}a`, ...full() }
    expect(writeMyPresets(s, 'u1', [mine])).toBe(true)
    expect(readMyPresets(s, 'u1')).toEqual([mine])
    expect(readMyPresets(s, 'u2')).toEqual([])
    expect(s.m.has(`${MY_PRESETS_STORAGE_PREFIX}u1`)).toBe(true)
  })

  it('leaves out anything that is not a preset the server would take', () => {
    const s = memStorage()
    const ok: TouchPreset = { id: `${MINE_PRESET_PREFIX}a`, ...full() }
    s.setItem(
      `${MY_PRESETS_STORAGE_PREFIX}u1`,
      JSON.stringify([
        ok,
        { ...ok, id: 'org:x' }, // not a member's own id
        { ...ok, id: `${MINE_PRESET_PREFIX}b`, name: '' }, // refused by the rules
        { id: `${MINE_PRESET_PREFIX}c` }, // not a preset
        ok, // a repeat
        null,
      ]),
    )
    expect(readMyPresets(s, 'u1')).toEqual([ok])
    s.setItem(`${MY_PRESETS_STORAGE_PREFIX}u1`, '{not json')
    expect(readMyPresets(s, 'u1')).toEqual([])
    const throwing = {
      getItem: () => {
        throw new Error('blocked')
      },
      setItem: () => {
        throw new Error('blocked')
      },
    }
    expect(readMyPresets(throwing, 'u1')).toEqual([])
    expect(writeMyPresets(throwing, 'u1', [ok])).toBe(false)
    expect(writeMyPresets(null, 'u1', [ok])).toBe(false)
  })

  it('keeps at most as many as an org may', () => {
    const s = memStorage()
    const many = Array.from({ length: LIMITS.mine + 5 }, (_, i) => ({ id: `${MINE_PRESET_PREFIX}${i}`, ...full() }))
    writeMyPresets(s, 'u1', many)
    expect(readMyPresets(s, 'u1')).toHaveLength(LIMITS.mine)
  })
})

describe('FR-111 P4b-2 drafts', () => {
  it('copies a built-in deeply under a new member id, never changing it', () => {
    const office = BUILTIN_PRESETS[0]
    const copy = copyForEdit(office, `Copy of ${office.name}`, () => 0.5)
    expect(copy.id.startsWith(MINE_PRESET_PREFIX)).toBe(true)
    expect(copy.id).not.toBe(office.id)
    expect(copy.name).toBe(`Copy of ${office.name}`)
    ;(copy.controls[0] as { id: string }).id = 'changed'
    expect(office.controls[0].id).not.toBe('changed')
    expect(copyForEdit(office, 'x'.repeat(60)).name).toHaveLength(LIMITS.name)
  })

  it('new controls and buttons take free ids and make a layout the server takes', () => {
    const controls: TouchControl[] = []
    controls.push(newRow(controls))
    controls.push(newRow(controls))
    controls.push(newDpad(controls))
    controls.push(newStick(controls))
    const row = controls[0] as Extract<TouchControl, { kind: 'row' }>
    row.buttons.push(newButton(controls, row.id))
    expect(controls.map((c) => c.id)).toEqual(['row1', 'row2', 'pad1', 'stick1'])
    expect(row.buttons.map((b) => b.id)).toEqual(['row1-b1', 'row1-b2'])
    expect(validatePreset({ name: 'New', controls })).toBeNull()
    expect(freeId(controls, 'row')).toBe('row3')
  })

  it('mints distinct member ids', () => {
    let n = 0.1
    const a = newMineId(() => (n += 0.1))
    const b = newMineId(() => (n += 0.1))
    expect(a).not.toBe(b)
    expect(a.startsWith(MINE_PRESET_PREFIX)).toBe(true)
  })
})
