import { describe, it, expect } from 'vitest'

// Pure helpers exported for testing. We can't import the full composable
// here without mocking the WS store; the helpers below are self-contained
// pure functions and are what actually determine the wire format, so they
// carry the important invariants.
import { browserButton, kbdCodeToHid } from '@/composables/useRemoteControl'

describe('browserButton', () => {
  it.each([
    [0, 'left'],
    [1, 'middle'],
    [2, 'right'],
    [3, 'back'],
    [4, 'forward'],
  ])('maps button %i → %s', (n, expected) => {
    expect(browserButton(n)).toBe(expected)
  })

  it('falls back to left for unknown button indices', () => {
    expect(browserButton(99)).toBe('left')
  })
})

describe('kbdCodeToHid', () => {
  it('maps all 26 letters to the HID keyboard/keypad page', () => {
    // 'KeyA' → 0x04, 'KeyZ' → 0x1d
    expect(kbdCodeToHid('KeyA')).toBe(0x04)
    expect(kbdCodeToHid('KeyM')).toBe(0x04 + 12)
    expect(kbdCodeToHid('KeyZ')).toBe(0x1d)
  })

  it('maps digits 1..9 → 0x1e..0x26 and 0 → 0x27', () => {
    expect(kbdCodeToHid('Digit1')).toBe(0x1e)
    expect(kbdCodeToHid('Digit9')).toBe(0x26)
    expect(kbdCodeToHid('Digit0')).toBe(0x27)
  })

  it('covers navigation + control keys', () => {
    expect(kbdCodeToHid('Enter')).toBe(0x28)
    expect(kbdCodeToHid('Escape')).toBe(0x29)
    expect(kbdCodeToHid('Backspace')).toBe(0x2a)
    expect(kbdCodeToHid('Tab')).toBe(0x2b)
    expect(kbdCodeToHid('Space')).toBe(0x2c)
    expect(kbdCodeToHid('ArrowRight')).toBe(0x4f)
    expect(kbdCodeToHid('ArrowLeft')).toBe(0x50)
    expect(kbdCodeToHid('ArrowDown')).toBe(0x51)
    expect(kbdCodeToHid('ArrowUp')).toBe(0x52)
    expect(kbdCodeToHid('Home')).toBe(0x4a)
    expect(kbdCodeToHid('End')).toBe(0x4d)
    expect(kbdCodeToHid('PageUp')).toBe(0x4b)
    expect(kbdCodeToHid('PageDown')).toBe(0x4e)
    expect(kbdCodeToHid('Insert')).toBe(0x49)
    expect(kbdCodeToHid('Delete')).toBe(0x4c)
  })

  it('maps F1..F12 to the HID function-key range', () => {
    expect(kbdCodeToHid('F1')).toBe(0x3a)
    expect(kbdCodeToHid('F5')).toBe(0x3e)
    expect(kbdCodeToHid('F12')).toBe(0x45)
  })

  it('maps all four sets of modifier keys (L and R)', () => {
    expect(kbdCodeToHid('ControlLeft')).toBe(0xe0)
    expect(kbdCodeToHid('ShiftLeft')).toBe(0xe1)
    expect(kbdCodeToHid('AltLeft')).toBe(0xe2)
    expect(kbdCodeToHid('MetaLeft')).toBe(0xe3)
    expect(kbdCodeToHid('ControlRight')).toBe(0xe4)
    expect(kbdCodeToHid('ShiftRight')).toBe(0xe5)
    expect(kbdCodeToHid('AltRight')).toBe(0xe6)
    expect(kbdCodeToHid('MetaRight')).toBe(0xe7)
  })

  it('returns null for unknown codes', () => {
    expect(kbdCodeToHid('BrowserBack')).toBeNull()
    expect(kbdCodeToHid('MediaPlayPause')).toBeNull()
    expect(kbdCodeToHid('GarbageCode')).toBeNull()
  })

  it('returns null for not-quite-matching shapes', () => {
    // Look-alikes that used to break naive startsWith checks.
    expect(kbdCodeToHid('Keyboard')).toBeNull() // too long for "Key_"
    expect(kbdCodeToHid('Digit10')).toBeNull() // digit out of single-char range
  })
})
