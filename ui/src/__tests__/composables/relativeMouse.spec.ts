// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import {
  agentSupportsRelativeMouse,
  createRelAccumulator,
  relativeAutoSwitchedOff,
  relativeButtonMessage,
  relativeMouseSwitchedOff,
  RELATIVE_AUTO_STORAGE_KEY,
  RELATIVE_MOUSE_STORAGE_KEY,
  shouldAutoEnterRelative,
} from '@/composables/relativeMouse'
import { createHeldInputTracker } from '@/composables/useRemoteControl'

describe('FR-109: relative mouse', () => {
  it('is offered only to an agent that advertises mouse-rel, by equality', () => {
    expect(agentSupportsRelativeMouse(['arbiter', 'exclusive', 'mouse-rel'])).toBe(true)
    // A prefix or a cousin is not the word: matching must stay equality.
    expect(agentSupportsRelativeMouse(['mouse-rel-x'])).toBe(false)
    expect(agentSupportsRelativeMouse(['mouse'])).toBe(false)
    expect(agentSupportsRelativeMouse(['Mouse-Rel'])).toBe(false)
    // An older agent says nothing, and an unknown list shape is no.
    expect(agentSupportsRelativeMouse(['arbiter', 'ghost-cursor'])).toBe(false)
    expect(agentSupportsRelativeMouse(undefined)).toBe(false)
    expect(agentSupportsRelativeMouse(null)).toBe(false)
  })

  it('reads the viewer kill switch, and storage that throws as not switched off', () => {
    const store = (v: string | null) => ({ getItem: (k: string) => (k === RELATIVE_MOUSE_STORAGE_KEY ? v : null) })
    expect(relativeMouseSwitchedOff(store('off'))).toBe(true)
    expect(relativeMouseSwitchedOff(store('on'))).toBe(false)
    expect(relativeMouseSwitchedOff(store(null))).toBe(false)
    expect(relativeMouseSwitchedOff(null)).toBe(false)
    const throwing = { getItem: () => { throw new Error('SecurityError') } }
    expect(relativeMouseSwitchedOff(throwing)).toBe(false)
  })

  it('sends whole counts and carries the fraction, so slow motion is not lost', () => {
    const acc = createRelAccumulator()
    acc.add(0.4, 0)
    expect(acc.take()).toBeNull()
    acc.add(0.7, 0)
    // 1.1 gathered: one count goes, 0.1 stays for the next send.
    expect(acc.take()).toEqual({ dx: 1, dy: 0 })
    acc.add(0.95, 0)
    expect(acc.take()).toEqual({ dx: 1, dy: 0 })
    // Twelve slow steps of a quarter count move three whole counts, not zero
    // (quarters are exact in binary, so this cannot flake on rounding).
    const slow = createRelAccumulator()
    let total = 0
    for (let i = 0; i < 12; i++) {
      slow.add(0.25, 0)
      total += slow.take()?.dx ?? 0
    }
    expect(total).toBe(3)
  })

  it('truncates toward zero in both directions and ignores what is not a number', () => {
    // Truncation, not rounding: 0.6 is not yet a count in either direction.
    const t = createRelAccumulator()
    t.add(0.6, -0.6)
    expect(t.take()).toBeNull()
    const acc = createRelAccumulator()
    acc.add(-1.5, 2.25)
    expect(acc.take()).toEqual({ dx: -1, dy: 2 })
    acc.add(-0.6, Number.NaN)
    // -0.5 carried plus -0.6 is -1.1: one count left, and NaN moved nothing.
    expect(acc.take()).toEqual({ dx: -1, dy: 0 })
    acc.reset()
    expect(acc.take()).toBeNull()
  })

  it('presses and lets go where the pointer is: no position on the wire', () => {
    expect(relativeButtonMessage('left', true)).toEqual({ t: 'mouse_press', btn: 'left' })
    expect(relativeButtonMessage('right', false)).toEqual({ t: 'mouse_release', btn: 'right' })
  })

  it('P3: a click switches only when the host hid its cursor and every gate agrees', () => {
    const ok = {
      allowed: true,
      autoOff: false,
      cursorHidden: true,
      cursorTracked: true,
      hostLocked: false,
      suppressed: false,
      relativeActive: false,
      inputGranted: true,
    }
    expect(shouldAutoEnterRelative(ok)).toBe(true)
    // Each gate alone keeps the click a click.
    expect(shouldAutoEnterRelative({ ...ok, allowed: false })).toBe(false)
    expect(shouldAutoEnterRelative({ ...ok, autoOff: true })).toBe(false)
    expect(shouldAutoEnterRelative({ ...ok, cursorHidden: false })).toBe(false)
    // A host whose tracker never saw a pointer (Linux without DRM capture,
    // macOS) sends one `cursor:hide` at the start and nothing after it: that
    // hide is not an app capturing the pointer, and its first click must stay
    // a click.
    expect(shouldAutoEnterRelative({ ...ok, cursorTracked: false })).toBe(false)
    // The lock screen hides the cursor too: a locked host never switches.
    expect(shouldAutoEnterRelative({ ...ok, hostLocked: true })).toBe(false)
    // Left with Esc while the cursor stayed hidden: not pulled back in.
    expect(shouldAutoEnterRelative({ ...ok, suppressed: true })).toBe(false)
    expect(shouldAutoEnterRelative({ ...ok, relativeActive: true })).toBe(false)
    expect(shouldAutoEnterRelative({ ...ok, inputGranted: false })).toBe(false)
  })

  it('P3: the auto-switch has its own kill switch', () => {
    const store = (v: string | null) => ({ getItem: (k: string) => (k === RELATIVE_AUTO_STORAGE_KEY ? v : null) })
    expect(relativeAutoSwitchedOff(store('off'))).toBe(true)
    expect(relativeAutoSwitchedOff(store(null))).toBe(false)
    expect(relativeAutoSwitchedOff({ getItem: () => { throw new Error('SecurityError') } })).toBe(false)
  })

  it('leaving Pointer Lock lets go of the mouse, not of a chord', () => {
    const held = createHeldInputTracker()
    held.key(0xe0, true)
    held.button('left', true)
    held.button('right', true)
    expect(held.releaseButtons()).toEqual(['left', 'right'])
    expect(held.size()).toBe(1)
    expect(held.releaseAll()).toEqual({ keys: [0xe0], buttons: [] })
  })
})
