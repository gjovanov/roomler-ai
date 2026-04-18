import { describe, it, expect } from 'vitest'

// Pure helpers exported for testing. We can't import the full composable
// here without mocking the WS store; the helpers below are self-contained
// pure functions and are what actually determine the wire format, so they
// carry the important invariants.
import { browserButton, kbdCodeToHid, letterboxedNormalise } from '@/composables/useRemoteControl'

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

describe('letterboxedNormalise', () => {
  // Frame is the outer .video-frame rect; video*  are the <video>'s
  // intrinsic dimensions. object-fit: contain letterboxes the content.
  const frame = { left: 0, top: 0, width: 2560, height: 1600 }

  it('center click at frame center is the video center when aspect matches', () => {
    // 2560×1600 video in 2560×1600 frame: no letterbox, center is 0.5/0.5.
    const r = letterboxedNormalise(1280, 800, frame, 2560, 1600)
    expect(r.x).toBeCloseTo(0.5, 6)
    expect(r.y).toBeCloseTo(0.5, 6)
    expect(r.insideVideo).toBe(true)
  })

  it('ignores clicks in top letterbox when video is wider than frame', () => {
    // 3840×2160 (16:9) in 2560×1600 (16:10) → 80 px black bar top + bottom.
    // A click at y=40 is inside the top letterbox.
    const r = letterboxedNormalise(100, 40, frame, 3840, 2160)
    expect(r.insideVideo).toBe(false)
  })

  it('clicks inside visible region of 16:9 → 16:10 letterbox normalise correctly', () => {
    // 3840×2160 video in 2560×1600 frame: visibleH = 2560/(16/9) = 1440;
    // top offset = (1600-1440)/2 = 80. Click at frame (1280, 800) is the
    // center: localY = 800-80 = 720, y = 720/1440 = 0.5. Good.
    const r = letterboxedNormalise(1280, 800, frame, 3840, 2160)
    expect(r.x).toBeCloseTo(0.5, 6)
    expect(r.y).toBeCloseTo(0.5, 6)
    expect(r.insideVideo).toBe(true)
  })

  it('clicks inside visible region of taller-than-wide video normalise correctly', () => {
    // 1080×1920 portrait video in 2560×1600 frame: visibleW = 1600*(1080/1920) = 900;
    // left offset = (2560-900)/2 = 830. Click at (830+450, 800) is center of content.
    const r = letterboxedNormalise(830 + 450, 800, frame, 1080, 1920)
    expect(r.x).toBeCloseTo(0.5, 6)
    expect(r.y).toBeCloseTo(0.5, 6)
    expect(r.insideVideo).toBe(true)
  })

  it('falls back to frame-relative coords before first decoded frame', () => {
    // videoWidth=0 means no stream intrinsic yet — normalise against frame.
    const r = letterboxedNormalise(640, 400, frame, 0, 0)
    expect(r.x).toBeCloseTo(0.25, 6)
    expect(r.y).toBeCloseTo(0.25, 6)
    expect(r.insideVideo).toBe(true)
  })

  it('clamps out-of-frame clicks to [0,1]', () => {
    const r = letterboxedNormalise(-100, 5000, frame, 2560, 1600)
    expect(r.x).toBeGreaterThanOrEqual(0)
    expect(r.x).toBeLessThanOrEqual(1)
    expect(r.y).toBeGreaterThanOrEqual(0)
    expect(r.y).toBeLessThanOrEqual(1)
  })
})
