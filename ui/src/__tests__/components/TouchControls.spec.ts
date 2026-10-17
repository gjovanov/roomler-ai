// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { nextTick } from 'vue'
import TouchControls from '@/components/remote/TouchControls.vue'
import { HID, presetById, type ControlsSink, type TouchPreset } from '@/composables/touchControls'

type Call = [string, ...unknown[]]

function sinkRecorder() {
  const calls: Call[] = []
  const sink: ControlsSink = {
    key: (k, d) => calls.push(['key', k, d]),
    button: (b, d) => calls.push(['button', b, d]),
    move: (dx, dy) => calls.push(['move', dx, dy]),
    begin: () => calls.push(['begin']),
    end: () => calls.push(['end']),
    width: () => 1000,
  }
  return { sink, calls }
}

function pointer(el: Element, type: string, init: { id: number; x?: number; y?: number }) {
  const ev = Object.assign(new Event(type, { bubbles: true, cancelable: true }), {
    pointerId: init.id,
    clientX: init.x ?? 0,
    clientY: init.y ?? 0,
  })
  el.dispatchEvent(ev)
}

/** A control 150 px across with its top-left corner at the origin. */
function placeAt(el: Element, size = 150) {
  vi.spyOn(el, 'getBoundingClientRect').mockReturnValue({
    left: 0,
    top: 0,
    right: size,
    bottom: size,
    width: size,
    height: size,
    x: 0,
    y: 0,
    toJSON: () => ({}),
  } as DOMRect)
}

let frames: FrameRequestCallback[] = []
let wrapper: VueWrapper | null = null

beforeEach(() => {
  frames = []
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => frames.push(cb))
  vi.stubGlobal('cancelAnimationFrame', () => { frames = [] })
})
afterEach(() => {
  wrapper?.unmount()
  wrapper = null
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

function mountWith(
  presetId: string,
  extra: Partial<{ ctrlIsCmd: boolean; scrollX: number; scrollY: number }> = {},
) {
  const { sink, calls } = sinkRecorder()
  const parentDown = vi.fn()
  const host = document.createElement('div')
  host.addEventListener('pointerdown', parentDown)
  host.addEventListener('pointermove', parentDown)
  host.addEventListener('pointerup', parentDown)
  document.body.appendChild(host)
  const w = mount(TouchControls, {
    props: { preset: presetById(presetId) as TouchPreset, sink, ...extra },
    attachTo: host,
  })
  wrapper = w
  return { calls, parentDown, w }
}

describe('TouchControls (FR-111 P3)', () => {
  it('lays out the preset, and a held chord presses in order and lets go in reverse', async () => {
    const { calls, w } = mountWith('office')
    expect(w.find('[data-testid="rc-touch-controls"]').attributes('data-preset')).toBe('office')
    const copy = w.find('[data-testid="rc-tc-office-copy"]')
    expect(copy.text()).toBe('Ctrl+C')
    pointer(copy.element, 'pointerdown', { id: 1 })
    await nextTick()
    expect(copy.classes()).toContain('tc-held')
    pointer(copy.element, 'pointerup', { id: 1 })
    await nextTick()
    expect(copy.classes()).not.toContain('tc-held')
    expect(calls).toEqual([
      ['key', HID.ControlLeft, true],
      ['key', HID.KeyC, true],
      ['key', HID.KeyC, false],
      ['key', HID.ControlLeft, false],
    ])
  })

  it('keeps a touch on a control away from the stage under it', () => {
    const { parentDown, w } = mountWith('office')
    const esc = w.find('[data-testid="rc-tc-office-esc"]').element
    pointer(esc, 'pointerdown', { id: 1 })
    pointer(esc, 'pointermove', { id: 1 })
    pointer(esc, 'pointerup', { id: 1 })
    expect(parentDown).not.toHaveBeenCalled()
  })

  it('says Cmd on a Mac host that turns Ctrl into Cmd', () => {
    const { w } = mountWith('office', { ctrlIsCmd: true })
    expect(w.find('[data-testid="rc-tc-office-copy"]').text()).toBe('⌘C')
  })

  it('the d-pad follows the finger: up, then up-right, then right', async () => {
    const { calls, w } = mountWith('wasd')
    const pad = w.find('[data-testid="rc-tc-move"]')
    placeAt(pad.element)
    pointer(pad.element, 'pointerdown', { id: 2, x: 75, y: 5 }) // the top arm
    pointer(pad.element, 'pointermove', { id: 2, x: 130, y: 20 }) // up-right
    await nextTick()
    expect(pad.findAll('.tc-arm.tc-held').map((a) => a.text()).sort()).toEqual(['D', 'W'])
    pointer(pad.element, 'pointermove', { id: 2, x: 145, y: 75 }) // right
    pointer(pad.element, 'pointercancel', { id: 2 })
    expect(calls).toEqual([
      ['key', HID.KeyW, true],
      ['key', HID.KeyD, true],
      ['key', HID.KeyW, false],
      ['key', HID.KeyD, false],
    ])
  })

  it('a held stick moves the pointer every frame until it is let go', () => {
    const { calls, w } = mountWith('wasd')
    const stick = w.find('[data-testid="rc-tc-look"]')
    placeAt(stick.element)
    const t0 = performance.now()
    pointer(stick.element, 'pointerdown', { id: 3, x: 150, y: 75 }) // full push right
    expect(calls).toEqual([['begin']])
    expect(frames.length).toBe(1)
    frames.shift()!(t0 + 50)
    const [kind, dx, dy] = calls[1]!
    expect(kind).toBe('move')
    expect(dx as number).toBeGreaterThan(0)
    expect(dy as number).toBeCloseTo(0)
    expect(frames.length).toBe(1) // still held: the next frame is asked for
    pointer(stick.element, 'pointerup', { id: 3 })
    expect(calls[calls.length - 1]).toEqual(['end'])
    frames.shift()!(t0 + 100)
    expect(frames.length).toBe(0) // let go: the loop ends
  })

  it('lets go of everything when the preset changes, the window blurs, or it unmounts', async () => {
    const { calls, w } = mountWith('office')
    pointer(w.find('[data-testid="rc-tc-office-tab"]').element, 'pointerdown', { id: 1 })
    // Cast: the hosted image's vue-tsc reads .vue through the env.d.ts shim,
    // where a real prop fails the excess-property check (#2205).
    await w.setProps({ preset: presetById('arrows') as TouchPreset } as unknown as Parameters<typeof w.setProps>[0])
    expect(calls).toEqual([
      ['key', HID.Tab, true],
      ['key', HID.Tab, false],
    ])
    pointer(w.find('[data-testid="rc-tc-keys-enter"]').element, 'pointerdown', { id: 4 })
    window.dispatchEvent(new Event('blur'))
    expect(calls.slice(2)).toEqual([
      ['key', HID.Enter, true],
      ['key', HID.Enter, false],
    ])
    pointer(w.find('[data-testid="rc-tc-keys-space"]').element, 'pointerdown', { id: 5 })
    w.unmount()
    wrapper = null
    expect(calls.slice(4)).toEqual([
      ['key', HID.Space, true],
      ['key', HID.Space, false],
    ])
  })
})
