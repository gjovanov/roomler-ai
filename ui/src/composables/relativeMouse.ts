// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// FR-109 — relative mouse: the pieces of Pointer Lock mode that need no DOM,
// so they can be tested as plain functions. The wiring (requestPointerLock,
// the input pump) lives in useRemoteControl.ts.

/** The agent's word for "I inject relative motion" in `AgentCaps.input`.
 *  Matched by equality, never as a prefix: an unknown word is ignored. */
export const MOUSE_REL_CAP = 'mouse-rel'

/** Viewer-side kill switch: `localStorage['roomler-rc-relative-mouse'] = 'off'`
 *  hides the relative-mouse control, whatever the agent advertises. */
export const RELATIVE_MOUSE_STORAGE_KEY = 'roomler-rc-relative-mouse'

/** Does this agent take `mouse_rel` / `mouse_press`? Only one that says so:
 *  an older agent cannot parse either frame. */
export function agentSupportsRelativeMouse(input: readonly string[] | null | undefined): boolean {
  return Array.isArray(input) && input.includes(MOUSE_REL_CAP)
}

/** The viewer's kill switch, read where the control is offered. Storage that
 *  throws (a private window, blocked site data) reads as "not switched off". */
export function relativeMouseSwitchedOff(storage: Pick<Storage, 'getItem'> | null | undefined): boolean {
  try {
    return storage?.getItem(RELATIVE_MOUSE_STORAGE_KEY) === 'off'
  } catch {
    return false
  }
}

/** Sums Pointer Lock deltas between sends. `take()` hands out whole counts
 *  and keeps the fraction for the next send, so slow motion (a high-DPI
 *  mouse reports fractional `movementX`) is never rounded away. */
export interface RelAccumulator {
  add(dx: number, dy: number): void
  /** The whole counts gathered so far, or `null` when there is nothing to
   *  send yet. */
  take(): { dx: number; dy: number } | null
  reset(): void
}

export function createRelAccumulator(): RelAccumulator {
  let x = 0
  let y = 0
  return {
    add(dx, dy) {
      if (Number.isFinite(dx)) x += dx
      if (Number.isFinite(dy)) y += dy
    },
    take() {
      const dx = Math.trunc(x)
      const dy = Math.trunc(y)
      if (dx === 0 && dy === 0) return null
      x -= dx
      y -= dy
      return { dx, dy }
    },
    reset() {
      x = 0
      y = 0
    },
  }
}

/** A button in relative mode presses and lets go WHERE THE POINTER IS:
 *  `mouse_button` carries a position every backend moves to first, and in
 *  Pointer Lock the viewer has none to send (the host app owns the cursor). */
export function relativeButtonMessage(btn: string, down: boolean): Record<string, unknown> {
  return down ? { t: 'mouse_press', btn } : { t: 'mouse_release', btn }
}
