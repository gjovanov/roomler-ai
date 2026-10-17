// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// FR-111 — touch on the stage: the pieces that need no DOM, so they can be
// tested as plain functions. The wiring (the stage's listeners, its
// `touch-action`) lives in useRemoteControl.ts and RemoteControl.vue.

/** Viewer-side kill switch: `localStorage['roomler-rc-touch'] = 'legacy'` gives
 *  the stage back to the browser's own touch handling (pan and zoom over the
 *  picture), as before FR-111. */
export const TOUCH_STORAGE_KEY = 'roomler-rc-touch'

/** The stage's `touch-action`. `none` hands every touch on the picture to the
 *  viewer: without it a touch browser takes a drag over as a pan, ends the
 *  pointer with `pointercancel`, and the press already forwarded to the host
 *  is never let go (FR-111 P0, §8 row 1). Storage that throws (a private
 *  window, blocked site data) reads as "not switched off". */
export function stageTouchAction(storage: Pick<Storage, 'getItem'> | null | undefined): 'none' | 'auto' {
  try {
    return storage?.getItem(TOUCH_STORAGE_KEY) === 'legacy' ? 'auto' : 'none'
  } catch {
    return 'none'
  }
}

/** Which button each pointer pressed on the host, so the end of that pointer
 *  (`pointerup`, or a `pointercancel` that never brings one) releases exactly
 *  what it pressed, and a pointer that pressed nothing releases nothing. */
export interface PointerPressTracker {
  press(pointerId: number, btn: string): void
  /** The button this pointer pressed, forgotten as it is returned; `undefined`
   *  when it pressed nothing (or was already released). */
  end(pointerId: number): string | undefined
  size(): number
}

export function createPointerPressTracker(): PointerPressTracker {
  const pressed = new Map<number, string>()
  return {
    press(pointerId: number, btn: string) {
      pressed.set(pointerId, btn)
    },
    end(pointerId: number) {
      const btn = pressed.get(pointerId)
      pressed.delete(pointerId)
      return btn
    },
    size() {
      return pressed.size
    },
  }
}

/** The frame that lets go of a cancelled pointer's press. A cancel carries no
 *  position worth trusting, so absolute mode releases at the last position the
 *  pointer reached; relative mode (FR-109) releases where the host's pointer
 *  is, with `mouse_release`, as every relative release does. */
export function releaseForCancelledPointer(
  btn: string,
  relative: boolean,
  last: { x: number; y: number },
):
  | { t: 'mouse_release'; btn: string }
  | { t: 'mouse_button'; btn: string; down: false; x: number; y: number; mon: 0 } {
  if (relative) return { t: 'mouse_release', btn }
  return { t: 'mouse_button', btn, down: false, x: last.x, y: last.y, mon: 0 }
}

/** FR-111 P2 — how a finger drives the host: `direct` (the finger is the
 *  pointer, today's) or `trackpad` (the screen is a laptop's trackpad). */
export type TouchMode = 'direct' | 'trackpad'

/** Where this browser remembers the touch mode. */
export const TOUCH_MODE_STORAGE_KEY = 'roomler-rc-touch-mode'

/** The remembered touch mode: `trackpad` only for that exact word, `direct`
 *  otherwise (the default until the operator decides FR-111's open decision 1),
 *  and `direct` when storage throws. */
export function readTouchMode(storage: Pick<Storage, 'getItem'> | null | undefined): TouchMode {
  try {
    return storage?.getItem(TOUCH_MODE_STORAGE_KEY) === 'trackpad' ? 'trackpad' : 'direct'
  } catch {
    return 'direct'
  }
}
