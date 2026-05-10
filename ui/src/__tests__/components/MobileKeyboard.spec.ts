import { describe, it, expect, beforeAll, vi } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import MobileKeyboard from '@/components/remote/MobileKeyboard.vue'

beforeAll(() => {
  // jsdom-friendly stubs the component reaches for during focus.
  if (!global.requestAnimationFrame) {
    global.requestAnimationFrame = ((cb: FrameRequestCallback) => {
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
  })
})
