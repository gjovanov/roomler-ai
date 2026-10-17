// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import {
  createPointerPressTracker,
  readTouchMode,
  releaseForCancelledPointer,
  stageTouchAction,
  TOUCH_MODE_STORAGE_KEY,
  TOUCH_STORAGE_KEY,
} from '@/composables/touchInput'

function storageWith(value: string | null): Pick<Storage, 'getItem'> {
  return { getItem: (k: string) => (k === TOUCH_STORAGE_KEY ? value : null) }
}

describe('FR-111 stageTouchAction', () => {
  it('hands every touch on the picture to the viewer by default', () => {
    expect(stageTouchAction(storageWith(null))).toBe('none')
    expect(stageTouchAction(null)).toBe('none')
    expect(stageTouchAction(undefined)).toBe('none')
  })

  it('gives the stage back to the browser only for the exact word', () => {
    expect(stageTouchAction(storageWith('legacy'))).toBe('auto')
    expect(stageTouchAction(storageWith('Legacy'))).toBe('none')
    expect(stageTouchAction(storageWith('off'))).toBe('none')
  })

  it('reads storage that throws as not switched off', () => {
    const throwing = {
      getItem: () => {
        throw new Error('SecurityError')
      },
    }
    expect(stageTouchAction(throwing)).toBe('none')
  })
})

describe('FR-111 createPointerPressTracker', () => {
  it('a cancelled pointer releases exactly the button it pressed', () => {
    const t = createPointerPressTracker()
    t.press(2, 'left')
    expect(t.end(2)).toBe('left')
    expect(t.size()).toBe(0)
  })

  it('a pointer that pressed nothing, or was already released, releases nothing', () => {
    const t = createPointerPressTracker()
    expect(t.end(7)).toBeUndefined()
    t.press(3, 'right')
    expect(t.end(3)).toBe('right')
    // The pointerup came first; a late cancel for the same pointer sends no second release.
    expect(t.end(3)).toBeUndefined()
  })

  it('keeps two fingers apart', () => {
    const t = createPointerPressTracker()
    t.press(4, 'left')
    t.press(5, 'right')
    expect(t.end(5)).toBe('right')
    expect(t.end(4)).toBe('left')
    expect(t.size()).toBe(0)
  })
})

describe('FR-111 releaseForCancelledPointer', () => {
  it('releases at the last point the pointer reached, never at the cancel position', () => {
    expect(releaseForCancelledPointer('left', false, { x: 0.35, y: 0.2 })).toEqual({
      t: 'mouse_button',
      btn: 'left',
      down: false,
      x: 0.35,
      y: 0.2,
      mon: 0,
    })
  })

  it('in relative mode releases where the host pointer is (FR-109)', () => {
    expect(releaseForCancelledPointer('left', true, { x: 0.35, y: 0.2 })).toEqual({
      t: 'mouse_release',
      btn: 'left',
    })
  })
})

describe('FR-111 readTouchMode', () => {
  const store = (v: string | null): Pick<Storage, 'getItem'> => ({
    getItem: (k: string) => (k === TOUCH_MODE_STORAGE_KEY ? v : null),
  })
  it('is direct unless this browser chose the trackpad (decision 1 pending)', () => {
    expect(readTouchMode(store(null))).toBe('direct')
    expect(readTouchMode(store('trackpad'))).toBe('trackpad')
    expect(readTouchMode(store('Trackpad'))).toBe('direct')
    expect(readTouchMode(null)).toBe('direct')
  })
  it('reads storage that throws as direct', () => {
    expect(readTouchMode({ getItem: () => { throw new Error('SecurityError') } })).toBe('direct')
  })
})
