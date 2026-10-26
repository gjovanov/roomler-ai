// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect, beforeAll, vi } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import MobileKeyboard from '@/components/remote/MobileKeyboard.vue'
import {
  applyKeyRoute,
  isViewerOwnInput,
  routeKeyEvent,
  type KeyRouteEffects,
  type KeyRouteState,
} from '@/composables/useRemoteControl'

beforeAll(() => {
  // jsdom-friendly stubs the component reaches for during focus.
  if (!globalThis.requestAnimationFrame) {
    globalThis.requestAnimationFrame = ((cb: FrameRequestCallback) => {
      cb(0)
      return 0
    }) as typeof requestAnimationFrame
  }
})

/**
 * Build a minimal `InputEvent`-shape that the component's `@input`
 * handler will accept. JSDOM's `InputEvent` constructor accepts
 * `inputType` but not always reliably across versions; using a plain
 * object cast avoids that fragility while still locking the contract
 * we care about (the handler reads `inputType` + the textarea's
 * value).
 */
function inputEvent(inputType: string): Event {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const e = new Event('input', { bubbles: true }) as any
  e.inputType = inputType
  return e as Event
}

describe('MobileKeyboard', () => {
  it('renders nothing when closed', () => {
    const w = mount(MobileKeyboard, { props: { open: false } })
    expect(w.find('[data-testid="mobile-keyboard"]').exists()).toBe(false)
  })

  it('renders the capture textarea + toolbar when open', () => {
    const w = mount(MobileKeyboard, { props: { open: true } })
    expect(w.find('[data-testid="mobile-keyboard"]').exists()).toBe(true)
    expect(w.find('textarea').exists()).toBe(true)
    expect(w.findAll('.mkb-btn').length).toBeGreaterThan(5)
  })

  it("marks its capture textarea as the HOST's surface, so the viewer's key listener keeps forwarding it (#2869)", () => {
    // The pairing with `isViewerOwnInput`: the window-level key listener
    // steps aside for every other editable element on the page (the files
    // drawer's path field typed into the host), but a keydown aimed at this
    // textarea must still reach the host — a Backspace on its always-empty
    // textarea raises no `input` event, so the keydown is the only way.
    const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
    const ta = w.find('textarea').element as HTMLTextAreaElement
    expect(ta.getAttribute('data-rc-capture')).toBe('soft-keyboard')
    expect(isViewerOwnInput(ta, null)).toBe(false)
    // Control: the same textarea without the mark would be the viewer's own.
    const plain = document.createElement('textarea')
    document.body.appendChild(plain)
    expect(isViewerOwnInput(plain, null)).toBe(true)
    plain.remove()
    w.unmount()
  })

  it('emits keyText on plain text typed (after composition skip)', async () => {
    const w = mount(MobileKeyboard, { props: { open: true } })
    const ta = w.find('textarea').element as HTMLTextAreaElement
    ta.value = 'hello'
    await w.find('textarea').trigger('input')
    const emitted = w.emitted('keyText')
    expect(emitted).toBeTruthy()
    expect(emitted?.[0]?.[0]).toBe('hello')
    // Buffer flushed after read so the next keystroke arrives in
    // isolation (Android Gboard quirk).
    expect(ta.value).toBe('')
  })

  it('does NOT emit keyText while composing (IME)', async () => {
    const w = mount(MobileKeyboard, { props: { open: true } })
    await w.find('textarea').trigger('compositionstart')
    const ta = w.find('textarea').element as HTMLTextAreaElement
    ta.value = 'pinyi'
    await w.find('textarea').trigger('input')
    expect(w.emitted('keyText')).toBeUndefined()
  })

  it('flushes composed string on compositionend', async () => {
    const w = mount(MobileKeyboard, { props: { open: true } })
    await w.find('textarea').trigger('compositionstart')
    await w.find('textarea').trigger('compositionend', { data: '你好' })
    const emitted = w.emitted('keyText')
    expect(emitted?.[0]?.[0]).toBe('你好')
  })

  it('routes deleteContentBackward to Backspace HID (0x2a)', async () => {
    const w = mount(MobileKeyboard, { props: { open: true } })
    const handler = w.vm
    // Trigger by direct emit-via-input-event simulation. We inject
    // the inputType directly because JSDOM's InputEvent constructor
    // doesn't always reliably set inputType.
    const ta = w.find('textarea').element as HTMLTextAreaElement
    ta.dispatchEvent(inputEvent('deleteContentBackward'))
    await flushPromises()
    void handler // satisfies the linter that we're still using vm
    const keyEmits = w.emitted('key')
    expect(keyEmits).toBeTruthy()
    // Two emits: down then up.
    expect(keyEmits).toHaveLength(2)
    const [code, down, mods] = keyEmits![0]
    expect(code).toBe(0x2a)
    expect(down).toBe(true)
    expect(mods).toBe(0)
  })

  it('routes insertLineBreak to Enter HID (0x28)', async () => {
    const w = mount(MobileKeyboard, { props: { open: true } })
    const ta = w.find('textarea').element as HTMLTextAreaElement
    ta.dispatchEvent(inputEvent('insertLineBreak'))
    await flushPromises()
    const emit = w.emitted('key')
    expect(emit).toBeTruthy()
    expect(emit![0][0]).toBe(0x28)
  })

  describe('special-key toolbar', () => {
    it('Esc button emits Esc HID (0x29) down + up', async () => {
      const w = mount(MobileKeyboard, { props: { open: true } })
      const escBtn = w.findAll('.mkb-btn').find((b) => b.text() === 'Esc')
      expect(escBtn).toBeTruthy()
      await escBtn!.trigger('pointerdown')
      const e = w.emitted('key')
      expect(e).toHaveLength(2)
      expect(e![0]).toEqual([0x29, true, 0])
      expect(e![1]).toEqual([0x29, false, 0])
    })

    it('arrow buttons emit correct HIDs', async () => {
      const w = mount(MobileKeyboard, { props: { open: true } })
      const left = w.findAll('.mkb-btn').find((b) => b.text() === '←')
      await left!.trigger('pointerdown')
      const e = w.emitted('key')
      // 0x50 = ArrowLeft per HID page 0x07
      expect(e![0][0]).toBe(0x50)
    })

    it('sticky Ctrl modifier arms then fires on next text input', async () => {
      const w = mount(MobileKeyboard, { props: { open: true } })
      // Tap Ctrl button.
      const ctrlBtn = w.findAll('.mkb-btn').find((b) => b.text() === 'Ctrl')
      await ctrlBtn!.trigger('pointerdown')
      // Now type 'v' (lowercase). Should emit Ctrl+V as a HID key
      // event (0x19 = KeyV per HID page 0x07), NOT plain keyText.
      const ta = w.find('textarea').element as HTMLTextAreaElement
      ta.value = 'v'
      await w.find('textarea').trigger('input')
      const keyEmits = w.emitted('key')
      expect(keyEmits).toBeTruthy()
      // Expect down + up of KeyV with Ctrl mod = 0x01.
      expect(keyEmits![0]).toEqual([0x19, true, 0x01])
      expect(keyEmits![1]).toEqual([0x19, false, 0x01])
      // Sticky-once: pinned mod cleared after fire — typing again
      // should produce plain keyText, no Ctrl.
      ta.value = 'a'
      await w.find('textarea').trigger('input')
      const text = w.emitted('keyText')
      expect(text).toBeTruthy()
      expect(text![0][0]).toBe('a')
    })

    it('sticky Ctrl tapped twice clears without firing', async () => {
      const w = mount(MobileKeyboard, { props: { open: true } })
      const ctrlBtn = w.findAll('.mkb-btn').find((b) => b.text() === 'Ctrl')
      await ctrlBtn!.trigger('pointerdown') // arm
      await ctrlBtn!.trigger('pointerdown') // disarm
      // Type plain text — should emit keyText, not key.
      const ta = w.find('textarea').element as HTMLTextAreaElement
      ta.value = 'a'
      await w.find('textarea').trigger('input')
      expect(w.emitted('key')).toBeFalsy()
      expect(w.emitted('keyText')).toBeTruthy()
    })

    it('hide button emits close', async () => {
      const w = mount(MobileKeyboard, { props: { open: true } })
      const close = w.findAll('.mkb-btn').find((b) => b.classes().includes('mkb-btn-close'))
      await close!.trigger('pointerdown')
      expect(w.emitted('close')).toBeTruthy()
    })
  })

  describe('keydown passthrough', () => {
    it('Tab key from a hardware keyboard routes through HID (not text)', async () => {
      const w = mount(MobileKeyboard, { props: { open: true } })
      await w.find('textarea').trigger('keydown', { key: 'Tab' })
      const e = w.emitted('key')
      expect(e).toBeTruthy()
      // 0x2b = Tab HID
      expect(e![0][0]).toBe(0x2b)
    })

    it('Escape key from a hardware keyboard routes through HID', async () => {
      const w = mount(MobileKeyboard, { props: { open: true } })
      await w.find('textarea').trigger('keydown', { key: 'Escape' })
      const e = w.emitted('key')
      expect(e![0][0]).toBe(0x29)
    })

    it('regular alphanumeric keys in keydown do NOT route to HID (textarea handles via input)', async () => {
      const w = mount(MobileKeyboard, { props: { open: true } })
      await w.find('textarea').trigger('keydown', { key: 'a' })
      // No HID emit — `a` flows through the textarea's own input
      // event path which we test separately.
      expect(w.emitted('key')).toBeFalsy()
    })

    it('a keydown someone already handled (defaultPrevented) is not the component\'s', async () => {
      // The second lock behind #2888: the viewer's listener prevents the
      // default of every key it forwards. Should it ever forward one without
      // stopping propagation, this handler must still not press it again.
      const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
      const ta = w.find('textarea').element as HTMLTextAreaElement
      const ev = new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter', bubbles: true, cancelable: true })
      ev.preventDefault()
      ta.dispatchEvent(ev)
      await flushPromises()
      expect(w.emitted('key')).toBeFalsy()
      w.unmount()
    })
  })

  describe("one owner per key, with the viewer's listener in front (#2888)", () => {
    // The listener as `attachInput` registers it — window, CAPTURE phase —
    // running the real `routeKeyEvent` / `applyKeyRoute` with recording
    // effects, ahead of the component's own handlers.
    function withListener(run: () => void, state: Partial<KeyRouteState> = {}): Record<string, unknown>[] {
      const sent: Record<string, unknown>[] = []
      const fx: KeyRouteEffects = {
        send: (m) => {
          sent.push(m)
        },
        hostKeyCode: (code) => code,
        held: () => {},
        heldCount: () => 0,
        releaseHeld: () => {},
        sendCtrlAltDel: () => {},
        deferPaste: () => {},
        mirrorClipboard: () => {},
      }
      const base: KeyRouteState = {
        pointerInside: false,
        keyboardLocked: false,
        keyboardPaused: false,
        surface: null,
        ...state,
      }
      const kd = (e: Event) => applyKeyRoute(routeKeyEvent(e as KeyboardEvent, true, base), e as KeyboardEvent, fx)
      const ku = (e: Event) => applyKeyRoute(routeKeyEvent(e as KeyboardEvent, false, base), e as KeyboardEvent, fx)
      window.addEventListener('keydown', kd, { capture: true })
      window.addEventListener('keyup', ku, { capture: true })
      try {
        run()
      } finally {
        window.removeEventListener('keydown', kd, { capture: true })
        window.removeEventListener('keyup', ku, { capture: true })
      }
      return sent
    }
    const press = (el: Element, init: KeyboardEventInit) => {
      el.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init }))
      el.dispatchEvent(new KeyboardEvent('keyup', { bubbles: true, cancelable: true, ...init }))
    }

    it("a hardware keyboard's Enter, Tab, Escape and arrows: the listener sends one press each, the component never sees them", async () => {
      const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
      const ta = w.find('textarea').element as HTMLTextAreaElement
      const keys = [
        ['Enter', 0x28],
        ['Tab', 0x2b],
        ['Escape', 0x29],
        ['ArrowLeft', 0x50],
        ['ArrowDown', 0x51],
      ] as const
      const sent = withListener(() => {
        for (const [key] of keys) press(ta, { key, code: key })
      })
      await flushPromises()
      // Before #2888 the component emitted a down + up pair for each of these
      // on top of the listener's frames: two presses per key on the host.
      expect(w.emitted('key')).toBeUndefined()
      expect(sent.filter((m) => m.down === true).map((m) => m.code)).toEqual(keys.map(([, code]) => code))
      expect(sent.filter((m) => m.down === false).map((m) => m.code)).toEqual(keys.map(([, code]) => code))
      w.unmount()
    })

    it("a hardware keyboard's Backspace and Ctrl+C: the listener's, as before (#2878)", async () => {
      const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
      const ta = w.find('textarea').element as HTMLTextAreaElement
      const sent = withListener(() => {
        press(ta, { key: 'Backspace', code: 'Backspace' })
        press(ta, { key: 'c', code: 'KeyC', ctrlKey: true })
      })
      await flushPromises()
      expect(w.emitted('key')).toBeUndefined()
      expect(sent).toEqual([
        { t: 'key', code: 0x2a, down: true, mods: 0 },
        { t: 'key', code: 0x2a, down: false, mods: 0 },
        { t: 'key', code: 0x06, down: true, mods: 1 },
        { t: 'key', code: 0x06, down: false, mods: 1 },
      ])
      w.unmount()
    })

    it("a hardware letter: the listener leaves it, and the textarea's input path types it once", async () => {
      const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
      const ta = w.find('textarea').element as HTMLTextAreaElement
      const sent = withListener(() => {
        const kd = new KeyboardEvent('keydown', { key: 'a', code: 'KeyA', bubbles: true, cancelable: true })
        ta.dispatchEvent(kd)
        // Untouched: the browser may insert it. jsdom has no default actions,
        // so the insertion is done by hand, as the browser would.
        expect(kd.defaultPrevented).toBe(false)
        ta.value = 'a'
        ta.dispatchEvent(new Event('input', { bubbles: true }))
        ta.dispatchEvent(new KeyboardEvent('keyup', { key: 'a', code: 'KeyA', bubbles: true, cancelable: true }))
      })
      await flushPromises()
      expect(sent).toEqual([])
      expect(w.emitted('keyText')).toEqual([['a']])
      expect(ta.value).toBe('')
      w.unmount()
    })

    it("the toolbar's sticky Ctrl, then a hardware c: Ctrl+C once, from the component, and the modifier cleared", async () => {
      // `consumeModBits` in the `input` handler is the only code that turns an
      // armed toolbar modifier into a chord — the one chord mechanism a phone
      // has. A listener that owned printable keys would type the "c" and leave
      // Ctrl armed for the next toolbar key.
      const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
      const ctrlBtn = w.findAll('.mkb-btn').find((b) => b.text() === 'Ctrl')
      await ctrlBtn!.trigger('pointerdown')
      expect(ctrlBtn!.classes()).toContain('mkb-btn-mod-active')
      const ta = w.find('textarea').element as HTMLTextAreaElement
      const sent = withListener(() => {
        const kd = new KeyboardEvent('keydown', { key: 'c', code: 'KeyC', bubbles: true, cancelable: true })
        ta.dispatchEvent(kd)
        expect(kd.defaultPrevented).toBe(false)
        ta.value = 'c'
        ta.dispatchEvent(new Event('input', { bubbles: true }))
        ta.dispatchEvent(new KeyboardEvent('keyup', { key: 'c', code: 'KeyC', bubbles: true, cancelable: true }))
      })
      await flushPromises()
      expect(sent).toEqual([])
      expect(w.emitted('key')).toEqual([
        [0x06, true, 0x01],
        [0x06, false, 0x01],
      ])
      expect(ctrlBtn!.classes()).not.toContain('mkb-btn-mod-active')
      // Sticky-once: the next letter is plain text.
      ta.value = 'a'
      ta.dispatchEvent(new Event('input', { bubbles: true }))
      await flushPromises()
      expect(w.emitted('keyText')).toEqual([['a']])
      w.unmount()
    })

    it("the OS keyboard's own Enter (no physical code) is the component's alone", async () => {
      const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
      const ta = w.find('textarea').element as HTMLTextAreaElement
      const sent = withListener(() => press(ta, { key: 'Enter', code: '' }))
      await flushPromises()
      expect(sent).toEqual([])
      expect(w.emitted('key')).toEqual([
        [0x28, true, 0],
        [0x28, false, 0],
      ])
      w.unmount()
    })

    it("the OS keyboard's own Backspace and Delete (no physical code) are the component's alone (#2909)", async () => {
      // The listener maps by `code` and drops them; the always-empty textarea
      // deletes nothing, so no `input` event carries them either. Before
      // #2909 they reached the host through nobody.
      const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
      const ta = w.find('textarea').element as HTMLTextAreaElement
      const sent = withListener(() => {
        press(ta, { key: 'Backspace', code: '' })
        press(ta, { key: 'Delete', code: '' })
      })
      await flushPromises()
      expect(sent).toEqual([])
      expect(w.emitted('key')).toEqual([
        [0x2a, true, 0],
        [0x2a, false, 0],
        [0x4c, true, 0],
        [0x4c, false, 0],
      ])
      w.unmount()
    })

    it('a Backspace an IME is composing with (keyCode 229) is neither owner\'s keydown', async () => {
      const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
      const ta = w.find('textarea').element as HTMLTextAreaElement
      const sent = withListener(() => press(ta, { key: 'Backspace', code: '', keyCode: 229 }))
      await flushPromises()
      expect(sent).toEqual([])
      expect(w.emitted('key')).toBeUndefined()
      w.unmount()
    })

    it("text the OS keyboard types (an input event, no keydown) is the component's alone", async () => {
      const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
      const ta = w.find('textarea').element as HTMLTextAreaElement
      const sent = withListener(() => {
        ta.value = 'hello'
        ta.dispatchEvent(new Event('input', { bubbles: true }))
      })
      await flushPromises()
      expect(sent).toEqual([])
      expect(w.emitted('keyText')).toEqual([['hello']])
      expect(ta.value).toBe('')
      w.unmount()
    })

    it('an IME composition: the listener drops its keydowns, the component emits the composed string once', async () => {
      const w = mount(MobileKeyboard, { props: { open: true }, attachTo: document.body })
      const ta = w.find('textarea').element as HTMLTextAreaElement
      const sent = withListener(() => {
        ta.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true }))
        press(ta, { key: 'n', code: 'KeyN', isComposing: true })
        ta.value = 'に'
        ta.dispatchEvent(new Event('input', { bubbles: true }))
        press(ta, { key: 'Enter', code: 'Enter', isComposing: true })
        ta.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '日本' }))
      })
      await flushPromises()
      expect(sent).toEqual([])
      expect(w.emitted('keyText')).toEqual([['日本']])
      expect(w.emitted('key')).toBeUndefined()
      w.unmount()
    })
  })
})
