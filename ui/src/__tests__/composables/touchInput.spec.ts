// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import {
  createPointerPressTracker,
  phoneSized,
  pictureOverflows,
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
  it('a remembered choice wins; with none, a phone-sized screen starts in trackpad (decision 1)', () => {
    // Nothing remembered: the screen decides.
    expect(readTouchMode(store(null))).toBe('direct')
    expect(readTouchMode(store(null), false)).toBe('direct')
    expect(readTouchMode(store(null), true)).toBe('trackpad')
    expect(readTouchMode(null, true)).toBe('trackpad')
    // The toolbar's choice is remembered as the exact word, and wins either way.
    expect(readTouchMode(store('trackpad'), false)).toBe('trackpad')
    expect(readTouchMode(store('direct'), true)).toBe('direct')
    // Any other word is nothing remembered.
    expect(readTouchMode(store('Trackpad'), false)).toBe('direct')
    expect(readTouchMode(store('Trackpad'), true)).toBe('trackpad')
  })
  it('reads storage that throws as nothing remembered', () => {
    const throwing = { getItem: () => { throw new Error('SecurityError') } }
    expect(readTouchMode(throwing)).toBe('direct')
    expect(readTouchMode(throwing, true)).toBe('trackpad')
  })
})

describe('FR-111 phoneSized (decision 1)', () => {
  it('is the screen whose shorter side is under 600 CSS px, either way round', () => {
    expect(phoneSized({ width: 412, height: 915 })).toBe(true)
    expect(phoneSized({ width: 915, height: 412 })).toBe(true)
    expect(phoneSized({ width: 599, height: 1000 })).toBe(true)
    expect(phoneSized({ width: 600, height: 1000 })).toBe(false)
    expect(phoneSized({ width: 768, height: 1024 })).toBe(false)
    expect(phoneSized({ width: 1920, height: 1080 })).toBe(false)
  })
  it('is not a phone when the screen is unknown or reads nonsense', () => {
    expect(phoneSized(null)).toBe(false)
    expect(phoneSized(undefined)).toBe(false)
    expect(phoneSized({ width: 0, height: 0 })).toBe(false)
    expect(phoneSized({ width: Number.NaN, height: 800 })).toBe(false)
  })
})

describe('FR-111 pictureOverflows (decision 4)', () => {
  const stage = (sw: number, cw: number, sh: number, ch: number) => ({ scrollWidth: sw, clientWidth: cw, scrollHeight: sh, clientHeight: ch })
  it('pans only a picture larger than the stage, on either axis', () => {
    expect(pictureOverflows(stage(800, 400, 600, 600))).toBe(true)
    expect(pictureOverflows(stage(400, 400, 900, 600))).toBe(true)
    expect(pictureOverflows(stage(400, 400, 600, 600))).toBe(false)
  })
  it('a pixel of layout rounding is not an overflow', () => {
    expect(pictureOverflows(stage(401, 400, 601, 600))).toBe(false)
    expect(pictureOverflows(stage(402, 400, 600, 600))).toBe(true)
  })
})
