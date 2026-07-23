import { describe, it, expect, beforeEach, afterAll } from 'vitest'

// Pure helpers exported for testing. We can't import the full composable
// here without mocking the WS store; the helpers below are self-contained
// pure functions and are what actually determine the wire format, so they
// carry the important invariants.
import {
  browserButton,
  kbdCodeToHid,
  letterboxedNormalise,
  directVideoNormalise,
  extractStatsSnapshot,
  inspectBrowserVideoCodecs,
  base64ToBytes,
  shouldPreventDefault,
  filterCapsByPreference,
  resolutionWireMessage,
  isWebCodecsSupported,
  isVp9_444DecodeSupported,
  isChromeWithBrokenScriptTransform,
  chunkClipboardText,
  sendClipboardWriteOverDc,
  CLIPBOARD_CHUNK_BYTES,
  CLIPBOARD_MAX_BYTES,
  CLIPBOARD_SINGLE_ENVELOPE_THRESHOLD_BYTES,
  VP9_444_DC_LABEL,
  VP9_444_DC_OPTIONS,
  readStoredAudioEnabled,
  persistAudioEnabled,
  audioRequestFields,
  shortCodecFromReceiver,
  codecFromSdp,
  decideKeyAction,
  RC_RECONNECT_LADDER_MS,
  nextReconnectDelayMs,
  nextDirPath,
  parseControlInbound,
  parseAppsListReply,
  parseAppsActionReply,
  appsListWireMessage,
  appsFocusWireMessage,
  appsLaunchWireMessage,
  decodeStatWireMessage,
  displayMatchWireMessage,
  pickAutoTransport,
  AV1_CODEC_STRING,
  priorityWireMessage,
  codecChoiceToSettings,
  settingsToCodecChoice,
  parseLocalRelayDescriptor,
  localRelayIceServer,
  type AutoTransportInputs,
  type KeyDecision,
  type RcCodecChoice,
} from '@/composables/useRemoteControl'
import { codecMimeForShort } from '@/workers/rc-webcodecs-worker'
import { parseFrameHeader, isKeyframe, shouldDecodeFrame } from '@/workers/rc-vp9-444-worker'
import { shouldDecodeFrame as shouldDecodeFrameHevc, classifyCrop } from '@/workers/rc-hevc-worker'

function keyEvent(code: string, mods: Partial<{ ctrl: boolean; alt: boolean; meta: boolean; shift: boolean }> = {}): KeyboardEvent {
  return {
    code,
    ctrlKey: !!mods.ctrl,
    altKey: !!mods.alt,
    metaKey: !!mods.meta,
    shiftKey: !!mods.shift,
  } as KeyboardEvent
}

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

  it('maps the punctuation row to HID usages 0x2d–0x38, 0x35', () => {
    expect(kbdCodeToHid('Backquote')).toBe(0x35)
    expect(kbdCodeToHid('Minus')).toBe(0x2d)
    expect(kbdCodeToHid('Equal')).toBe(0x2e)
    expect(kbdCodeToHid('BracketLeft')).toBe(0x2f)
    expect(kbdCodeToHid('BracketRight')).toBe(0x30)
    expect(kbdCodeToHid('Backslash')).toBe(0x31)
    expect(kbdCodeToHid('Semicolon')).toBe(0x33)
    expect(kbdCodeToHid('Quote')).toBe(0x34)
    expect(kbdCodeToHid('Comma')).toBe(0x36)
    expect(kbdCodeToHid('Period')).toBe(0x37)
    expect(kbdCodeToHid('Slash')).toBe(0x38)
    expect(kbdCodeToHid('IntlBackslash')).toBe(0x64)
  })

  it('maps lock + system keys', () => {
    expect(kbdCodeToHid('CapsLock')).toBe(0x39)
    expect(kbdCodeToHid('NumLock')).toBe(0x53)
    expect(kbdCodeToHid('ScrollLock')).toBe(0x47)
    expect(kbdCodeToHid('PrintScreen')).toBe(0x46)
    expect(kbdCodeToHid('Pause')).toBe(0x48)
    expect(kbdCodeToHid('ContextMenu')).toBe(0x65)
  })

  it('maps the numeric keypad', () => {
    expect(kbdCodeToHid('NumpadDivide')).toBe(0x54)
    expect(kbdCodeToHid('NumpadMultiply')).toBe(0x55)
    expect(kbdCodeToHid('NumpadSubtract')).toBe(0x56)
    expect(kbdCodeToHid('NumpadAdd')).toBe(0x57)
    expect(kbdCodeToHid('NumpadEnter')).toBe(0x58)
    expect(kbdCodeToHid('NumpadDecimal')).toBe(0x63)
    expect(kbdCodeToHid('Numpad1')).toBe(0x59)
    expect(kbdCodeToHid('Numpad9')).toBe(0x61)
    expect(kbdCodeToHid('Numpad0')).toBe(0x62)
  })
})

/**
 * Decision tree that routes a `KeyboardEvent` to either the
 * layout-agnostic KeyText path or the existing HID Key path. Lock the
 * specific routing rules (AltGr, IME, chord-vs-printable, Tab carve-
 * out, keyup suppression) so future regressions in the rule set fail
 * loudly here rather than silently in the field.
 */
describe('decideKeyAction', () => {
  type EvShape = {
    key: string
    code: string
    ctrlKey?: boolean
    altKey?: boolean
    metaKey?: boolean
    shiftKey?: boolean
    isComposing?: boolean
    keyCode?: number
  }
  function ev(shape: EvShape) {
    return {
      key: shape.key,
      code: shape.code,
      ctrlKey: !!shape.ctrlKey,
      altKey: !!shape.altKey,
      metaKey: !!shape.metaKey,
      shiftKey: !!shape.shiftKey,
      isComposing: !!shape.isComposing,
      keyCode: shape.keyCode ?? 0,
    }
  }
  const altGr = (on: boolean) => (k: string) => k === 'AltGraph' && on

  it('US Shift+@ routes via KeyText (no real chord, just Shift)', () => {
    const r = decideKeyAction(
      ev({ key: '@', code: 'Digit2', shiftKey: true }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'text', text: '@' })
  })

  it('Shift+A (capital letter) routes via KeyText', () => {
    const r = decideKeyAction(
      ev({ key: 'A', code: 'KeyA', shiftKey: true }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'text', text: 'A' })
  })

  it('DEU/AT AltGr+Q (= "@") routes via KeyText — AltGraph carve-out', () => {
    // Browsers report AltGr as ctrlKey + altKey. Without the
    // AltGraph signal, this would mis-classify as a Ctrl+Alt+Q chord.
    const r = decideKeyAction(
      ev({ key: '@', code: 'KeyQ', ctrlKey: true, altKey: true }),
      true,
      altGr(true),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'text', text: '@' })
  })

  it('Ctrl+C preserves the chord on the HID path (0.1.34 fix lives here)', () => {
    const r = decideKeyAction(
      ev({ key: 'c', code: 'KeyC', ctrlKey: true }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({
      kind: 'key',
      code: 0x06,
      down: true,
      mods: 0x01,
    })
  })

  it('US-layout intentional Ctrl+Alt+Q stays on the HID path', () => {
    // Real chord: no AltGraph modifier, ev.key reflects the chord
    // (browsers leave it as 'q' for letter chords).
    const r = decideKeyAction(
      ev({ key: 'q', code: 'KeyQ', ctrlKey: true, altKey: true }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({
      kind: 'key',
      code: 0x14,
      down: true,
      mods: 0x05, // Ctrl | Alt
    })
  })

  it('keyup of a printable+nochord key emits no message', () => {
    const r = decideKeyAction(
      ev({ key: '@', code: 'Digit2', shiftKey: true }),
      false,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'drop' })
  })

  it('Enter routes via HID', () => {
    const r = decideKeyAction(
      ev({ key: 'Enter', code: 'Enter' }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'key', code: 0x28, down: true, mods: 0 })
  })

  it('Tab routes via HID even though ev.key is single-char "\\t"', () => {
    // Tab needs a real WM_KEYDOWN(VK_TAB) on the remote so apps that
    // gate focus traversal on it pick it up. KeyText would inject U+0009
    // which doesn't trigger focus change in many forms / IDEs.
    const r = decideKeyAction(
      ev({ key: '\t', code: 'Tab' }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'key', code: 0x2b, down: true, mods: 0 })
  })

  it('Space routes via KeyText (length-1 printable)', () => {
    // Pin the choice: if we want Space on the HID path later, this
    // test fails and forces an explicit decision.
    const r = decideKeyAction(
      ev({ key: ' ', code: 'Space' }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'text', text: ' ' })
  })

  it('IME composition (isComposing=true) drops without emitting', () => {
    const r = decideKeyAction(
      ev({ key: 'a', code: 'KeyA', isComposing: true }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'drop' })
  })

  it('Chromium IME placeholder (key="Process", keyCode=229) drops', () => {
    const r = decideKeyAction(
      ev({ key: 'Process', code: 'KeyA', keyCode: 229 }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'drop' })
  })

  it('Auto-repeat: each repeated keydown emits its own KeyText', () => {
    // Browsers fire keydown repeatedly while held. Emit one KeyText
    // per fire — matches local typing behaviour.
    const e = ev({ key: 'a', code: 'KeyA' })
    const r1 = decideKeyAction(e, true, altGr(false))
    const r2 = decideKeyAction(e, true, altGr(false))
    expect(r1).toEqual<KeyDecision>({ kind: 'text', text: 'a' })
    expect(r2).toEqual<KeyDecision>({ kind: 'text', text: 'a' })
  })

  it('Dead-key first stroke (key="Dead") falls to HID via Backquote', () => {
    // Pressing the dead-tilde on a US-International layout. Browsers
    // emit key="Dead" then later emit key="ñ" once the combine fires.
    // The first stroke isn't printable; it should hit the HID path,
    // which only works because Backquote is now in kbdCodeToHid.
    const r = decideKeyAction(
      ev({ key: 'Dead', code: 'Backquote' }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'key', code: 0x35, down: true, mods: 0 })
  })

  it('Combined dead-key result (key="ñ") routes via KeyText', () => {
    const r = decideKeyAction(
      ev({ key: 'ñ', code: 'KeyN' }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'text', text: 'ñ' })
  })

  it('Unmapped non-printable keys drop without error', () => {
    // BrowserBack has no HID mapping; nothing to send.
    const r = decideKeyAction(
      ev({ key: 'BrowserBack', code: 'BrowserBack' }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({ kind: 'drop' })
  })

  it('Cmd+C on macOS (metaKey) is a real chord', () => {
    // Browsers report Cmd as metaKey. Even with no Ctrl/Alt, metaKey
    // alone counts as a chord — Cmd+C must round-trip as a chord.
    const r = decideKeyAction(
      ev({ key: 'c', code: 'KeyC', metaKey: true }),
      true,
      altGr(false),
    )
    expect(r).toEqual<KeyDecision>({
      kind: 'key',
      code: 0x06,
      down: true,
      mods: 0x08, // Meta
    })
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

describe('extractStatsSnapshot', () => {
  /**
   * Build a fake `RTCStatsReport`: a `Map<string, RTCStats>` with the
   * shape the browser emits. Only the fields the helper reads are
   * populated; missing fields exercise the helper's fallback paths.
   */
  function makeReport(
    entries: Array<[string, Record<string, unknown>]>,
  ): RTCStatsReport {
    const map = new Map<string, Record<string, unknown>>()
    for (const [id, stats] of entries) map.set(id, { id, ...stats })
    return map as unknown as RTCStatsReport
  }

  it('returns bitrate=0 on first call (no previous snapshot)', () => {
    const report = makeReport([
      ['RTCInboundVideo_0', {
        type: 'inbound-rtp', kind: 'video',
        bytesReceived: 100_000, timestamp: 1_000, framesPerSecond: 30,
        codecId: 'Codec_H264',
      }],
      ['Codec_H264', { type: 'codec', mimeType: 'video/H264' }],
    ])
    const snap = extractStatsSnapshot(report, 0, 0)
    expect(snap.next.bitrate_bps).toBe(0)
    expect(snap.next.fps).toBe(30)
    expect(snap.next.codec).toBe('H264')
    expect(snap.bytes).toBe(100_000)
    expect(snap.tsMs).toBe(1_000)
  })

  it('computes bitrate from byte/timestamp delta on second call', () => {
    // 100 KB new bytes over 500 ms = 100_000 bytes × 8 / 0.5 s = 1_600_000 bps
    const report = makeReport([
      ['RTCInboundVideo_0', {
        type: 'inbound-rtp', kind: 'video',
        bytesReceived: 200_000, timestamp: 1_500, framesPerSecond: 59.9,
        codecId: 'Codec_H265',
      }],
      ['Codec_H265', { type: 'codec', mimeType: 'video/H265' }],
    ])
    const snap = extractStatsSnapshot(report, 100_000, 1_000)
    expect(snap.next.bitrate_bps).toBe(1_600_000)
    expect(snap.next.fps).toBe(59.9)
    expect(snap.next.codec).toBe('H265')
  })

  it('rounds fps to one decimal', () => {
    const report = makeReport([
      ['RTCInboundVideo_0', {
        type: 'inbound-rtp', kind: 'video',
        bytesReceived: 0, timestamp: 0, framesPerSecond: 29.876,
        codecId: 'Codec_X',
      }],
      ['Codec_X', { type: 'codec', mimeType: 'video/AV1' }],
    ])
    const snap = extractStatsSnapshot(report, 0, 0)
    expect(snap.next.fps).toBe(29.9)
    expect(snap.next.codec).toBe('AV1')
  })

  it('treats missing codec entry as empty string', () => {
    const report = makeReport([
      ['RTCInboundVideo_0', {
        type: 'inbound-rtp', kind: 'video',
        bytesReceived: 0, timestamp: 1_000, framesPerSecond: 0,
        codecId: 'Codec_Unmatched',
      }],
      // no `Codec_Unmatched` entry
    ])
    const snap = extractStatsSnapshot(report, 0, 0)
    expect(snap.next.codec).toBe('')
  })

  it('ignores non-video inbound-rtp (audio) and non-inbound streams', () => {
    const report = makeReport([
      ['RTCInboundAudio_0', {
        type: 'inbound-rtp', kind: 'audio',
        bytesReceived: 99_999, timestamp: 1_000,
      }],
      ['RTCOutboundVideo_0', {
        type: 'outbound-rtp', kind: 'video',
        bytesSent: 12_345, timestamp: 1_000,
      }],
    ])
    const snap = extractStatsSnapshot(report, 0, 0)
    expect(snap.bytes).toBe(0)
    expect(snap.tsMs).toBe(0)
    expect(snap.next).toEqual({ bitrate_bps: 0, fps: 0, codec: '' })
  })

  it('clamps negative byte deltas to 0 (e.g. counter reset on renegotiation)', () => {
    const report = makeReport([
      ['RTCInboundVideo_0', {
        type: 'inbound-rtp', kind: 'video',
        bytesReceived: 500, timestamp: 2_000, framesPerSecond: 30,
        codecId: 'Codec_H264',
      }],
      ['Codec_H264', { type: 'codec', mimeType: 'video/H264' }],
    ])
    const snap = extractStatsSnapshot(report, 1_000_000, 1_000)
    expect(snap.next.bitrate_bps).toBe(0)
  })

  it('strips the "video/" prefix case-insensitively', () => {
    const report = makeReport([
      ['RTCInboundVideo_0', {
        type: 'inbound-rtp', kind: 'video',
        bytesReceived: 0, timestamp: 0, framesPerSecond: 0,
        codecId: 'C',
      }],
      ['C', { type: 'codec', mimeType: 'VIDEO/VP9' }],
    ])
    const snap = extractStatsSnapshot(report, 0, 0)
    expect(snap.next.codec).toBe('VP9')
  })
})

describe('inspectBrowserVideoCodecs', () => {
  // Stub `RTCRtpReceiver.getCapabilities` for each test case. jsdom
  // (vitest's default DOM) doesn't ship a real WebRTC API.
  const realRTC = (globalThis as unknown as { RTCRtpReceiver?: unknown }).RTCRtpReceiver

  function stubCapabilities(codecs: Array<{ mimeType: string }>) {
    ;(globalThis as unknown as { RTCRtpReceiver: unknown }).RTCRtpReceiver = {
      getCapabilities: (kind: string) => {
        if (kind !== 'video') return null
        return { codecs }
      },
    }
  }

  function unsetCapabilities() {
    delete (globalThis as unknown as { RTCRtpReceiver?: unknown }).RTCRtpReceiver
  }

  beforeEach(() => {
    unsetCapabilities()
  })

  afterAll(() => {
    if (realRTC) {
      ;(globalThis as unknown as { RTCRtpReceiver: unknown }).RTCRtpReceiver = realRTC
    } else {
      unsetCapabilities()
    }
  })

  it('returns empty array when getCapabilities is unavailable', () => {
    expect(inspectBrowserVideoCodecs()).toEqual([])
  })

  it('returns empty array when getCapabilities returns no codecs', () => {
    stubCapabilities([])
    expect(inspectBrowserVideoCodecs()).toEqual([])
  })

  it('extracts known codecs and strips the video/ prefix', () => {
    stubCapabilities([
      { mimeType: 'video/H264' },
      { mimeType: 'video/VP8' },
    ])
    const out = inspectBrowserVideoCodecs()
    expect(out.sort()).toEqual(['h264', 'vp8'])
  })

  it('deduplicates multiple profile-level-id variants of the same codec', () => {
    stubCapabilities([
      { mimeType: 'video/H264' },
      { mimeType: 'video/H264' },
      { mimeType: 'video/H264' },
    ])
    expect(inspectBrowserVideoCodecs()).toEqual(['h264'])
  })

  it('filters out RTP mechanism codecs (rtx, red, ulpfec)', () => {
    stubCapabilities([
      { mimeType: 'video/H264' },
      { mimeType: 'video/rtx' },
      { mimeType: 'video/red' },
      { mimeType: 'video/ulpfec' },
      { mimeType: 'video/flexfec-03' },
    ])
    expect(inspectBrowserVideoCodecs()).toEqual(['h264'])
  })

  it('handles all five negotiable codecs', () => {
    stubCapabilities([
      { mimeType: 'video/H264' },
      { mimeType: 'video/H265' },
      { mimeType: 'video/AV1' },
      { mimeType: 'video/VP9' },
      { mimeType: 'video/VP8' },
    ])
    expect(inspectBrowserVideoCodecs().sort()).toEqual([
      'av1',
      'h264',
      'h265',
      'vp8',
      'vp9',
    ])
  })
})

describe('base64ToBytes', () => {
  it('round-trips an all-zero buffer', () => {
    // 8 zero bytes → base64 "AAAAAAAAAAA="
    const bytes = base64ToBytes('AAAAAAAAAAA=')
    expect(bytes.length).toBe(8)
    for (const b of bytes) expect(b).toBe(0)
  })

  it('decodes a single-byte buffer', () => {
    // 0xFF → "/w=="
    expect(Array.from(base64ToBytes('/w=='))).toEqual([0xff])
  })

  it('decodes a BGRA cursor-shape-sized buffer', () => {
    // 32×32 BGRA = 4096 bytes of alternating pattern. Agent encodes
    // this as base64 over the `cursor` data channel (1E.2); the
    // decoder must round-trip byte-exactly.
    const raw = new Uint8Array(4096)
    for (let i = 0; i < raw.length; i++) raw[i] = (i * 31) & 0xff
    // encode via btoa
    let bin = ''
    for (const b of raw) bin += String.fromCharCode(b)
    const b64 = btoa(bin)
    const out = base64ToBytes(b64)
    expect(out.length).toBe(raw.length)
    for (let i = 0; i < raw.length; i++) expect(out[i]).toBe(raw[i])
  })
})

describe('shouldPreventDefault', () => {
  it('always intercepts Tab', () => {
    expect(shouldPreventDefault(keyEvent('Tab'), false)).toBe(true)
    expect(shouldPreventDefault(keyEvent('Tab'), true)).toBe(true)
  })

  it('intercepts plain Backspace but not when modifiers are held', () => {
    expect(shouldPreventDefault(keyEvent('Backspace'), false)).toBe(true)
    expect(shouldPreventDefault(keyEvent('Backspace', { ctrl: true }), false)).toBe(false)
    expect(shouldPreventDefault(keyEvent('Backspace', { alt: true }), false)).toBe(false)
    expect(shouldPreventDefault(keyEvent('Backspace', { meta: true }), false)).toBe(false)
  })

  it('intercepts browser-eaten shortcuts only when pointer is over video', () => {
    for (const code of ['KeyA', 'KeyC', 'KeyV', 'KeyX', 'KeyZ', 'KeyY', 'KeyF', 'KeyS', 'KeyP', 'KeyR']) {
      // Pointer outside the viewer → the controller still gets normal
      // browser shortcuts (Ctrl+T to open a tab, etc.).
      expect(shouldPreventDefault(keyEvent(code, { ctrl: true }), false)).toBe(false)
      // Pointer over the viewer → intercept so the shortcut forwards
      // to the remote without triggering the local browser UI.
      expect(shouldPreventDefault(keyEvent(code, { ctrl: true }), true)).toBe(true)
      // Cmd (meta) is accepted as the same prefix on macOS.
      expect(shouldPreventDefault(keyEvent(code, { meta: true }), true)).toBe(true)
    }
  })

  it('lets untouched Ctrl+T / Ctrl+W through even when pointer inside', () => {
    // Explicitly NOT in the intercept list — these are still the user's
    // own browser tab/window controls. Forwarding them to the remote
    // over the input DC is fine, but we don't want to also preventDefault.
    expect(shouldPreventDefault(keyEvent('KeyT', { ctrl: true }), true)).toBe(false)
    expect(shouldPreventDefault(keyEvent('KeyW', { ctrl: true }), true)).toBe(false)
  })

  it('does not intercept a bare letter keypress without modifiers', () => {
    expect(shouldPreventDefault(keyEvent('KeyA'), true)).toBe(false)
    expect(shouldPreventDefault(keyEvent('KeyZ', { shift: true }), true)).toBe(false)
  })
})

describe('filterCapsByPreference', () => {
  const all = ['av1', 'h265', 'vp9', 'h264', 'vp8']

  it('passes the full list through when no override is set', () => {
    expect(filterCapsByPreference(all, null)).toEqual(all)
  })

  it('narrows to the preferred codec plus H.264 as a parachute', () => {
    expect(filterCapsByPreference(all, 'h265')).toEqual(['h265', 'h264'])
    expect(filterCapsByPreference(all, 'av1')).toEqual(['av1', 'h264'])
    expect(filterCapsByPreference(all, 'vp9')).toEqual(['vp9', 'h264'])
  })

  it('omits the H.264 parachute when H.264 is the preference itself', () => {
    expect(filterCapsByPreference(all, 'h264')).toEqual(['h264'])
  })

  it('returns just the preferred codec when the browser does not support H.264', () => {
    expect(filterCapsByPreference(['h265', 'vp9'], 'h265')).toEqual(['h265'])
  })

  it('falls back to just the H.264 parachute when the preferred codec is absent but H.264 is available', () => {
    expect(filterCapsByPreference(['h264', 'vp9'], 'av1')).toEqual(['h264'])
  })

  it('returns empty when neither the preferred codec nor H.264 is advertised', () => {
    // Forcing AV1 on a Firefox that offers neither AV1 nor H.264 will
    // fail to negotiate. That's by design — the operator sees the
    // filtered caps in the console log and can clear the override.
    expect(filterCapsByPreference(['vp9'], 'av1')).toEqual([])
  })
})

describe('directVideoNormalise', () => {
  // Viewer pixel (clientX, clientY) → [0,1] normalised, mapped against a
  // video element whose bounding rect is known. This is the mapper used
  // for `scale-original` and `scale-custom` modes, where no letterbox
  // math is needed because the <video> is rendered at its intrinsic
  // (scroll + scale) dimensions.

  const rect = { left: 10, top: 20, width: 1920, height: 1080 }

  it('maps top-left to (0,0)', () => {
    expect(directVideoNormalise(10, 20, rect)).toEqual({
      x: 0, y: 0, insideVideo: true,
    })
  })

  it('maps bottom-right to (1,1)', () => {
    const out = directVideoNormalise(1930, 1100, rect)
    expect(out.x).toBeCloseTo(1, 6)
    expect(out.y).toBeCloseTo(1, 6)
    expect(out.insideVideo).toBe(true)
  })

  it('reports outside when the pointer is before the rect', () => {
    const out = directVideoNormalise(0, 0, rect)
    expect(out.insideVideo).toBe(false)
    // Coordinates clamped to [0,1] regardless.
    expect(out.x).toBe(0)
    expect(out.y).toBe(0)
  })

  it('reports outside when the pointer is past the rect', () => {
    const out = directVideoNormalise(2500, 2500, rect)
    expect(out.insideVideo).toBe(false)
    expect(out.x).toBe(1)
    expect(out.y).toBe(1)
  })

  it('works at custom-scale sizes — mapping stays [0,1] vs the rendered rect', () => {
    // Remote is 1920x1080, custom scale 200% → rendered 3840x2160.
    // Middle of that rendered rect should still normalise to (0.5, 0.5)
    // — the agent doesn't care about scale; it gets normalised coords.
    const scaled = { left: 0, top: 0, width: 3840, height: 2160 }
    const out = directVideoNormalise(1920, 1080, scaled)
    expect(out.x).toBeCloseTo(0.5, 6)
    expect(out.y).toBeCloseTo(0.5, 6)
    expect(out.insideVideo).toBe(true)
  })

  it('returns a safe fallback when the rect has zero dimensions', () => {
    const zero = { left: 0, top: 0, width: 0, height: 0 }
    expect(directVideoNormalise(100, 100, zero)).toEqual({
      x: 0, y: 0, insideVideo: false,
    })
  })
})

describe('resolutionWireMessage', () => {
  // Locks the exact JSON shape the agent's control-DC handler parses.
  // Changing these assertions without changing the agent-side
  // `rc:resolution` match arms in `peer.rs::attach_control_handler`
  // will break the feature in the field.

  it('emits original with no dims', () => {
    expect(resolutionWireMessage({ mode: 'original' })).toEqual({
      t: 'rc:resolution',
      mode: 'original',
    })
  })

  it('emits fit with width + height', () => {
    expect(resolutionWireMessage({ mode: 'fit', width: 1920, height: 1080 })).toEqual({
      t: 'rc:resolution',
      mode: 'fit',
      width: 1920,
      height: 1080,
    })
  })

  it('emits custom with width + height', () => {
    expect(resolutionWireMessage({ mode: 'custom', width: 2560, height: 1440 })).toEqual({
      t: 'rc:resolution',
      mode: 'custom',
      width: 2560,
      height: 1440,
    })
  })

  it('rounds non-integer dims to the nearest pixel', () => {
    // devicePixelRatio + rect math can produce fractional CSS pixels.
    // The wire format is u32 — round at the browser boundary.
    expect(resolutionWireMessage({ mode: 'fit', width: 1920.7, height: 1080.2 })).toEqual({
      t: 'rc:resolution',
      mode: 'fit',
      width: 1921,
      height: 1080,
    })
  })

  it('drops invalid custom/fit with missing or zero dims', () => {
    expect(resolutionWireMessage({ mode: 'fit' })).toBeNull()
    expect(resolutionWireMessage({ mode: 'custom', width: 0, height: 100 })).toBeNull()
    expect(resolutionWireMessage({ mode: 'custom', width: 100, height: 0 })).toBeNull()
  })
})

describe('codecMimeForShort', () => {
  it('maps the known codec short-names to permissive WebCodecs strings', () => {
    expect(codecMimeForShort('h264')).toBe('avc1.42E01F')
    expect(codecMimeForShort('h265')).toBe('hev1.1.6.L153.B0')
    expect(codecMimeForShort('hevc')).toBe('hev1.1.6.L153.B0')
    expect(codecMimeForShort('av1')).toBe('av01.0.08M.08')
    expect(codecMimeForShort('vp9')).toBe('vp09.00.10.08')
    expect(codecMimeForShort('vp8')).toBe('vp8')
  })

  it('is case-insensitive', () => {
    expect(codecMimeForShort('H264')).toBe('avc1.42E01F')
    expect(codecMimeForShort('HEVC')).toBe('hev1.1.6.L153.B0')
  })

  it('falls back to H.264 for unknown short-names so a typo or stale wire value still produces a valid decoder config', () => {
    expect(codecMimeForShort('bogus')).toBe('avc1.42E01F')
    expect(codecMimeForShort('')).toBe('avc1.42E01F')
  })
})

describe('isWebCodecsSupported', () => {
  const originalTransform = (globalThis as unknown as { RTCRtpScriptTransform?: unknown }).RTCRtpScriptTransform
  const originalDecoder = (globalThis as unknown as { VideoDecoder?: unknown }).VideoDecoder
  afterAll(() => {
    ;(globalThis as unknown as Record<string, unknown>).RTCRtpScriptTransform = originalTransform
    ;(globalThis as unknown as Record<string, unknown>).VideoDecoder = originalDecoder
  })

  it('returns false when either API is missing (jsdom baseline)', () => {
    delete (globalThis as unknown as Record<string, unknown>).RTCRtpScriptTransform
    delete (globalThis as unknown as Record<string, unknown>).VideoDecoder
    expect(isWebCodecsSupported()).toBe(false)
  })

  it('returns false when only one of the two is present — Firefox-like with VideoDecoder but no RTCRtpScriptTransform', () => {
    ;(globalThis as unknown as Record<string, unknown>).RTCRtpScriptTransform = undefined
    ;(globalThis as unknown as Record<string, unknown>).VideoDecoder = function VideoDecoder() {}
    expect(isWebCodecsSupported()).toBe(false)
  })

  it('returns true when both APIs are constructors — Chrome 94+ surface', () => {
    ;(globalThis as unknown as Record<string, unknown>).RTCRtpScriptTransform = function RTCRtpScriptTransform() {}
    ;(globalThis as unknown as Record<string, unknown>).VideoDecoder = function VideoDecoder() {}
    expect(isWebCodecsSupported()).toBe(true)
  })
})

describe('isVp9_444DecodeSupported', () => {
  const originalDecoder = (globalThis as unknown as { VideoDecoder?: unknown }).VideoDecoder
  afterAll(() => {
    ;(globalThis as unknown as Record<string, unknown>).VideoDecoder = originalDecoder
  })

  it('returns false when VideoDecoder is missing (Firefox / older Safari)', async () => {
    delete (globalThis as unknown as Record<string, unknown>).VideoDecoder
    await expect(isVp9_444DecodeSupported()).resolves.toBe(false)
  })

  it('returns false when VideoDecoder lacks isConfigSupported', async () => {
    ;(globalThis as unknown as Record<string, unknown>).VideoDecoder = function VideoDecoder() {}
    await expect(isVp9_444DecodeSupported()).resolves.toBe(false)
  })

  it('queries isConfigSupported with the canonical VP9 profile 1 8-bit codec string and returns its supported flag', async () => {
    let observedCodec = ''
    ;(globalThis as unknown as Record<string, unknown>).VideoDecoder = {
      isConfigSupported: async (cfg: { codec: string }) => {
        observedCodec = cfg.codec
        return { supported: true }
      },
    }
    await expect(isVp9_444DecodeSupported()).resolves.toBe(true)
    // vp09.<profile=01>.<level=10>.<bit_depth=08> — Profile 1 is the
    // 4:4:4 path; locking the exact string keeps the worker's
    // VideoDecoder.configure call in lockstep with this probe.
    expect(observedCodec).toBe('vp09.01.10.08')
  })

  it('returns false when isConfigSupported reports unsupported', async () => {
    ;(globalThis as unknown as Record<string, unknown>).VideoDecoder = {
      isConfigSupported: async () => ({ supported: false }),
    }
    await expect(isVp9_444DecodeSupported()).resolves.toBe(false)
  })

  it('swallows isConfigSupported throws and returns false', async () => {
    ;(globalThis as unknown as Record<string, unknown>).VideoDecoder = {
      isConfigSupported: async () => { throw new Error('boom') },
    }
    await expect(isVp9_444DecodeSupported()).resolves.toBe(false)
  })
})

describe('isChromeWithBrokenScriptTransform (rc.43)', () => {
  const originalNav = globalThis.navigator
  function setNav(stub: unknown) {
    Object.defineProperty(globalThis, 'navigator', {
      value: stub,
      configurable: true,
      writable: true,
    })
  }
  afterAll(() => {
    Object.defineProperty(globalThis, 'navigator', {
      value: originalNav,
      configurable: true,
      writable: true,
    })
  })

  it('returns false when userAgentData brand is Chrome 147', () => {
    setNav({
      userAgentData: { brands: [{ brand: 'Google Chrome', version: '147' }] },
      userAgent: 'Mozilla/5.0 (Windows NT 10.0) Chrome/147.0.0.0',
    })
    expect(isChromeWithBrokenScriptTransform()).toBe(false)
  })

  it('returns true when userAgentData brand is Chromium 148 (field repro)', () => {
    setNav({
      userAgentData: {
        brands: [
          { brand: 'Chromium', version: '148' },
          { brand: 'Google Chrome', version: '148' },
          { brand: 'Not/A)Brand', version: '99' },
        ],
      },
      userAgent: 'Mozilla/5.0 (Windows NT 10.0) Chrome/148.0.0.0',
    })
    expect(isChromeWithBrokenScriptTransform()).toBe(true)
  })

  it('returns true when userAgentData missing but userAgent reports Chrome 149', () => {
    setNav({
      userAgent: 'Mozilla/5.0 (Windows NT 10.0) Chrome/149.0.7000.0',
    })
    expect(isChromeWithBrokenScriptTransform()).toBe(true)
  })

  it('returns false when neither brands nor Chrome UA token are present (Firefox/Safari)', () => {
    setNav({
      userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X) Firefox/130.0',
    })
    expect(isChromeWithBrokenScriptTransform()).toBe(false)
  })

  it('returns false when navigator is undefined (worker / SSR)', () => {
    setNav(undefined)
    expect(isChromeWithBrokenScriptTransform()).toBe(false)
  })
})

describe('chunkClipboardText (rc.44)', () => {
  it('returns a single-element array for empty input', () => {
    expect(chunkClipboardText('')).toEqual([''])
  })

  it('returns the input as a single chunk when it fits the budget', () => {
    expect(chunkClipboardText('hello')).toEqual(['hello'])
  })

  it('splits long ASCII at CLIPBOARD_CHUNK_BYTES boundaries', () => {
    const text = 'a'.repeat(CLIPBOARD_CHUNK_BYTES * 3 + 7)
    const chunks = chunkClipboardText(text)
    expect(chunks.length).toBe(4)
    // Every chunk except possibly the last is exactly CHUNK_BYTES bytes.
    const enc = new TextEncoder()
    for (let i = 0; i < chunks.length - 1; i++) {
      expect(enc.encode(chunks[i]).byteLength).toBeLessThanOrEqual(CLIPBOARD_CHUNK_BYTES)
    }
    // Round-trip: simple concatenation reproduces the input.
    expect(chunks.join('')).toBe(text)
  })

  it('preserves UTF-8 codepoint boundaries even when a 4-byte char straddles the split', () => {
    // Fill to (CHUNK_BYTES - 2) ASCII, then insert a 4-byte codepoint
    // (🦀). Natural split at CHUNK_BYTES would land inside the crab;
    // chunker must walk back to keep the codepoint whole.
    const prefix = 'a'.repeat(CLIPBOARD_CHUNK_BYTES - 2)
    const text = prefix + '🦀b'
    const chunks = chunkClipboardText(text)
    expect(chunks.join('')).toBe(text)
    // Each chunk must be a valid UTF-8 string (no replacement chars).
    const dec = new TextDecoder('utf-8', { fatal: true })
    const enc = new TextEncoder()
    for (const c of chunks) {
      // Round-tripping through fatal decoder throws on partial sequences.
      expect(() => dec.decode(enc.encode(c))).not.toThrow()
      expect(enc.encode(c).byteLength).toBeLessThanOrEqual(CLIPBOARD_CHUNK_BYTES)
    }
  })

  it('handles an entirely multi-byte payload (all-emoji stress test)', () => {
    // 5000 × 🦀 = 20000 bytes UTF-8 > CHUNK_BYTES (14336)
    const text = '🦀'.repeat(5000)
    const chunks = chunkClipboardText(text)
    expect(chunks.length).toBeGreaterThanOrEqual(2)
    expect(chunks.join('')).toBe(text)
    const enc = new TextEncoder()
    for (const c of chunks) {
      expect(enc.encode(c).byteLength).toBeLessThanOrEqual(CLIPBOARD_CHUNK_BYTES)
    }
  })
})

describe('sendClipboardWriteOverDc (rc.44)', () => {
  // Stub DC capturing every `.send()` call so we can assert wire shape
  // without a real RTCDataChannel. The function under test only reads
  // `readyState` indirectly (not at all in the body) — caller's
  // responsibility — so we don't need to stub that.
  function makeStubDc() {
    const sent: string[] = []
    return {
      sent,
      ch: {
        send: (s: string) => {
          sent.push(s)
        },
      } as unknown as RTCDataChannel,
    }
  }

  it('uses single-envelope clipboard:write for small ASCII text', () => {
    const { ch, sent } = makeStubDc()
    const n = sendClipboardWriteOverDc(ch, 'hello world')
    expect(n).toBe(1)
    expect(sent.length).toBe(1)
    const parsed = JSON.parse(sent[0])
    expect(parsed.t).toBe('clipboard:write')
    expect(parsed.text).toBe('hello world')
    expect(parsed.id).toBeUndefined()
  })

  it('uses single-envelope clipboard:write right at the threshold', () => {
    const { ch, sent } = makeStubDc()
    const text = 'a'.repeat(CLIPBOARD_SINGLE_ENVELOPE_THRESHOLD_BYTES)
    const n = sendClipboardWriteOverDc(ch, text)
    expect(n).toBe(1)
    const parsed = JSON.parse(sent[0])
    expect(parsed.t).toBe('clipboard:write')
  })

  it('switches to clipboard:write-chunk when above the threshold', () => {
    const { ch, sent } = makeStubDc()
    const text = 'a'.repeat(CLIPBOARD_SINGLE_ENVELOPE_THRESHOLD_BYTES + 1)
    const n = sendClipboardWriteOverDc(ch, text)
    expect(n).toBeGreaterThanOrEqual(1)
    expect(sent.length).toBe(n)
    const first = JSON.parse(sent[0])
    expect(first.t).toBe('clipboard:write-chunk')
    expect(typeof first.id).toBe('string')
    expect(first.seq).toBe(0)
    expect(first.last).toBe(n === 1)
  })

  it('chunked envelopes share an id and have sequential seq + final last=true', () => {
    const { ch, sent } = makeStubDc()
    // Force 3+ chunks: 3 × CHUNK_BYTES of ASCII.
    const text = 'a'.repeat(CLIPBOARD_CHUNK_BYTES * 3)
    sendClipboardWriteOverDc(ch, text)
    const envelopes = sent.map((s) => JSON.parse(s))
    const ids = new Set(envelopes.map((e) => e.id))
    expect(ids.size).toBe(1)
    envelopes.forEach((e, i) => {
      expect(e.t).toBe('clipboard:write-chunk')
      expect(e.seq).toBe(i)
      expect(e.last).toBe(i + 1 === envelopes.length)
    })
    // Concatenation of `text` across chunks must reproduce the input —
    // this is the load-bearing invariant for the agent's reassembler.
    expect(envelopes.map((e) => e.text).join('')).toBe(text)
  })

  it('truncates and warns when input exceeds the 1 MB hard cap', () => {
    const { ch, sent } = makeStubDc()
    const text = 'a'.repeat(CLIPBOARD_MAX_BYTES + 50_000)
    const warnings: string[] = []
    const originalWarn = console.warn
    console.warn = (...args: unknown[]) => {
      warnings.push(args.join(' '))
    }
    try {
      sendClipboardWriteOverDc(ch, text)
    } finally {
      console.warn = originalWarn
    }
    expect(warnings.some((w) => w.includes('truncated'))).toBe(true)
    const envelopes = sent.map((s) => JSON.parse(s))
    const totalBytes = envelopes
      .map((e) => new TextEncoder().encode(e.text as string).byteLength)
      .reduce((a, b) => a + b, 0)
    expect(totalBytes).toBeLessThanOrEqual(CLIPBOARD_MAX_BYTES)
  })

  it('every chunked envelope JSON string stays under 16 KB (SCTP ceiling proxy)', () => {
    const { ch, sent } = makeStubDc()
    // Mix of multi-byte chars to verify envelope overhead + UTF-8
    // expansion stays within the budget on a realistic input.
    const text = ('🦀'.repeat(2000) + 'ASCII filler '.repeat(2000) + '中文测试 '.repeat(1000))
    sendClipboardWriteOverDc(ch, text)
    const enc = new TextEncoder()
    for (const s of sent) {
      // The agent's webrtc-rs SCTP has max_message_size=65536; we
      // budget aggressively under that for headroom.
      expect(enc.encode(s).byteLength).toBeLessThan(16 * 1024)
    }
  })
})

describe('VP9_444_DC_LABEL + VP9_444_DC_OPTIONS', () => {
  // The agent's `on_data_channel` arm matches on `"video-bytes"`
  // exactly (see agents/roomler-agent/src/peer.rs:494). A typo on
  // either side silently turns the entire VP9-444 path into a
  // log-only dead end, so lock the value here.
  it('uses the exact label the agent matches on', () => {
    expect(VP9_444_DC_LABEL).toBe('video-bytes')
  })

  // Reliable + ordered: SCTP retransmits dropped chunks (a P-frame
  // hole would force the decoder to wait for the next IDR), and
  // libvpx wants frames in encode order. Don't relax these without
  // also bumping the worker assembler tests.
  it('uses the reliable + ordered DC profile', () => {
    expect(VP9_444_DC_OPTIONS).toEqual({ ordered: true })
  })
})

describe('audio opt-in (persist + request wire shape)', () => {
  beforeEach(() => {
    globalThis.localStorage?.clear()
  })

  it('defaults OFF when nothing is stored', () => {
    expect(readStoredAudioEnabled()).toBe(false)
  })

  it('round-trips through persistAudioEnabled', () => {
    persistAudioEnabled(true)
    expect(readStoredAudioEnabled()).toBe(true)
    persistAudioEnabled(false)
    expect(readStoredAudioEnabled()).toBe(false)
  })

  it('treats any non-"1" stored value as OFF (only the exact flag is truthy)', () => {
    globalThis.localStorage?.setItem('roomler-rc-audio-enabled', 'true')
    expect(readStoredAudioEnabled()).toBe(false)
  })

  // The agent's `rc:session.request` handler reads `audio_enabled`
  // (bool) with `#[serde(default)]` — omitting it must mean "no audio".
  // Lock the EXACT field name + presence so a rename on either side is
  // caught here rather than surfacing as silent no-audio in the field.
  it('emits { audio_enabled: true } only when enabled', () => {
    expect(audioRequestFields(true)).toEqual({ audio_enabled: true })
  })

  it('emits an empty object (field omitted) when disabled', () => {
    expect(audioRequestFields(false)).toEqual({})
  })
})

describe('shortCodecFromReceiver', () => {
  function makeReceiver(mime: string | undefined): Pick<RTCRtpReceiver, 'getParameters'> {
    return {
      getParameters: () => ({
        codecs: mime === undefined ? [] : [{ mimeType: mime }],
      } as unknown as RTCRtpSendParameters),
    }
  }

  it('returns h264 when the receiver is null or has no negotiated codec', () => {
    expect(shortCodecFromReceiver(null)).toBe('h264')
    expect(shortCodecFromReceiver(undefined)).toBe('h264')
    expect(shortCodecFromReceiver(makeReceiver(undefined))).toBe('h264')
  })

  it('maps common mime types to their short names', () => {
    expect(shortCodecFromReceiver(makeReceiver('video/H264'))).toBe('h264')
    expect(shortCodecFromReceiver(makeReceiver('video/H265'))).toBe('h265')
    expect(shortCodecFromReceiver(makeReceiver('video/hevc'))).toBe('h265')
    expect(shortCodecFromReceiver(makeReceiver('video/AV1'))).toBe('av1')
    expect(shortCodecFromReceiver(makeReceiver('video/VP9'))).toBe('vp9')
    expect(shortCodecFromReceiver(makeReceiver('video/VP8'))).toBe('vp8')
  })

  it('defaults to h264 when the mime is unrecognised', () => {
    expect(shortCodecFromReceiver(makeReceiver('video/random-codec'))).toBe('h264')
  })
})

describe('codecFromSdp', () => {
  const hevcAnswer = [
    'v=0',
    'o=- 1234 1 IN IP4 127.0.0.1',
    's=-',
    't=0 0',
    'a=group:BUNDLE 0',
    'm=video 9 UDP/TLS/RTP/SAVPF 101 96',
    'c=IN IP4 0.0.0.0',
    'a=rtpmap:101 H265/90000',
    'a=fmtp:101 profile-id=1',
    'a=rtpmap:96 H264/90000',
    'a=sendonly',
  ].join('\r\n')

  const h264Answer = [
    'v=0',
    'm=video 9 UDP/TLS/RTP/SAVPF 96 101',
    'a=rtpmap:96 H264/90000',
    'a=rtpmap:101 H265/90000',
  ].join('\n')

  it('picks the codec matching the first PT on the video m-line', () => {
    // HEVC answer — first PT on m=video is 101 → H265.
    expect(codecFromSdp(hevcAnswer)).toBe('h265')
    // H.264 answer — first PT is 96 → H264.
    expect(codecFromSdp(h264Answer)).toBe('h264')
  })

  it('handles LF-only line endings (some SDP mungers strip CRs)', () => {
    expect(codecFromSdp(h264Answer)).toBe('h264')
  })

  it('recognises the common short names', () => {
    const sdp = (codec: string) =>
      `m=video 9 UDP/TLS/RTP/SAVPF 101\r\na=rtpmap:101 ${codec}/90000\r\n`
    expect(codecFromSdp(sdp('H264'))).toBe('h264')
    expect(codecFromSdp(sdp('H265'))).toBe('h265')
    expect(codecFromSdp(sdp('HEVC'))).toBe('h265')
    expect(codecFromSdp(sdp('AV1'))).toBe('av1')
    expect(codecFromSdp(sdp('VP9'))).toBe('vp9')
    expect(codecFromSdp(sdp('VP8'))).toBe('vp8')
  })

  it('returns null when no video m-line is present', () => {
    expect(codecFromSdp('v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n')).toBeNull()
  })

  it('returns null when the matching rtpmap is missing', () => {
    expect(codecFromSdp('m=video 9 UDP/TLS/RTP/SAVPF 101\r\n')).toBeNull()
  })

  it('returns null for null/undefined/empty input', () => {
    expect(codecFromSdp(null)).toBeNull()
    expect(codecFromSdp(undefined)).toBeNull()
    expect(codecFromSdp('')).toBeNull()
  })

  it('returns null for an unknown codec short name', () => {
    expect(codecFromSdp('m=video 9 X 101\r\na=rtpmap:101 WEIRD/90000\r\n')).toBeNull()
  })
})

describe('rc-vp9-444-worker frame header', () => {
  // Lock the wire format so any change to the agent-side encoder
  // emit gets caught here. Schema: u32 size LE + u8 flags + u64 ts LE.

  function buildHeader(size: number, flags: number, ts: bigint): Uint8Array {
    const buf = new Uint8Array(13)
    const view = new DataView(buf.buffer)
    view.setUint32(0, size, true)
    view.setUint8(4, flags)
    view.setUint32(5, Number(ts & 0xffffffffn), true)
    view.setUint32(9, Number(ts >> 32n), true)
    return buf
  }

  it('parses size + flags + timestamp from a 13-byte header', () => {
    const header = buildHeader(1234, 0x01, 1_700_000_000_000_000n)
    const parsed = parseFrameHeader(header)
    expect(parsed).not.toBeNull()
    expect(parsed!.payloadSize).toBe(1234)
    expect(parsed!.flags).toBe(0x01)
    expect(parsed!.timestampUs).toBe(1_700_000_000_000_000n)
  })

  it('returns null when the input is shorter than the 13-byte header', () => {
    expect(parseFrameHeader(new Uint8Array(0))).toBeNull()
    expect(parseFrameHeader(new Uint8Array(12))).toBeNull()
  })

  it('decodes the keyframe flag bit', () => {
    expect(isKeyframe(0x00)).toBe(false)
    expect(isKeyframe(0x01)).toBe(true)
    // Higher bits reserved — keyframe bit is bit 0 only.
    expect(isKeyframe(0x02)).toBe(false)
    expect(isKeyframe(0x03)).toBe(true)
  })

  it('handles a zero-payload header without throwing', () => {
    const header = buildHeader(0, 0x00, 0n)
    const parsed = parseFrameHeader(header)
    expect(parsed).not.toBeNull()
    expect(parsed!.payloadSize).toBe(0)
  })

  it('round-trips a maximum-realistic 4K-keyframe size', () => {
    // 4K I444 worst-case keyframe is ~6 MB; spec allows up to 16 MB
    // before the worker rejects. Verify the parser doesn't choke at
    // that scale.
    const header = buildHeader(8_000_000, 0x01, 0n)
    const parsed = parseFrameHeader(header)
    expect(parsed!.payloadSize).toBe(8_000_000)
  })
})

describe('leading-delta keyframe gate (rc.103)', () => {
  // Locks the fix for the LAPTOP-P2TU89GB hevc_qsv failure: the HW decoder
  // throws "A key frame is required after configure() or flush()" on a
  // leading delta, and the FFmpeg async encoder can ship a buffered delta
  // ahead of the DC-open IDR. The worker must DROP deltas until the first
  // keyframe, then decode everything.
  for (const [label, gate] of [
    ['vp9-444', shouldDecodeFrame],
    ['hevc', shouldDecodeFrameHevc],
  ] as const) {
    describe(label, () => {
      it('drops a leading delta before any keyframe is seen', () => {
        expect(gate(false, false)).toBe(false)
      })

      it('always accepts a keyframe (the resync/start point)', () => {
        expect(gate(false, true)).toBe(true)
      })

      it('accepts deltas once a keyframe has been seen', () => {
        expect(gate(true, false)).toBe(true)
      })

      it('keeps accepting keyframes mid-stream', () => {
        expect(gate(true, true)).toBe(true)
      })

      it('models the DC-open race: drop deltas, latch on the IDR, then flow', () => {
        // Stream the worker would assemble right after "DC opened" on an
        // async encoder: a few buffered deltas, then the forced IDR, then
        // normal GOP. The gate state flips on the first keyframe.
        const stream = [false, false, false, true, false, false, true, false]
        let seen = false
        const decoded: boolean[] = []
        for (const isKey of stream) {
          const accept = gate(seen, isKey)
          decoded.push(accept)
          if (accept && isKey) seen = true
        }
        // First three leading deltas dropped; everything from the IDR on decodes.
        expect(decoded).toEqual([false, false, false, true, true, true, true, true])
      })
    })
  }
})

describe('classifyCrop — HEVC conformance-window handling', () => {
  // Locks the DESKTOP-V6FJE58 fix: QSV codes a 1920×1080 desktop as
  // 1920×1088 + an 8-row bottom crop (alignment padding = per-frame junk).
  // The NVDEC-bug rewrap (rc.102) must NOT override that legit crop — doing
  // so painted the junk rows as a purple/blue band flickering during drags.
  const rect = (width: number, height: number, x = 0, y = 0) => ({ x, y, width, height })

  it('trusts the QSV 1080p alignment crop (coded 1088 → visible 1080)', () => {
    expect(classifyCrop(1920, 1088, rect(1920, 1080))).toBe('alignment')
  })

  it('still rewraps the NVDEC misreported-geometry bug (2560×1600 → 1280×720)', () => {
    expect(classifyCrop(2560, 1600, rect(1280, 720))).toBe('spurious')
  })

  it('reports exact when visibleRect equals the coded size', () => {
    expect(classifyCrop(1920, 1200, rect(1920, 1200))).toBe('exact')
  })

  it('reports exact when geometry is missing (closed/exotic frames)', () => {
    expect(classifyCrop(0, 0, rect(1920, 1080))).toBe('exact')
    expect(classifyCrop(1920, 1080, null)).toBe('exact')
  })

  it('treats an offset-origin crop as spurious (alignment crops anchor at 0,0)', () => {
    expect(classifyCrop(1920, 1088, rect(1912, 1080, 8, 0))).toBe('spurious')
  })

  it('draws the alignment boundary at one CTU (64px)', () => {
    // 63 = largest possible alignment pad (dim ≡ 1 mod 64); a ≥64 deficit
    // can only be a genuinely smaller picture → NVDEC-bug territory.
    expect(classifyCrop(1920, 1088, rect(1920, 1025))).toBe('alignment')
    expect(classifyCrop(1920, 1088, rect(1920, 1024))).toBe('spurious')
    expect(classifyCrop(1920, 1088, rect(1856, 1080))).toBe('spurious')
  })

  it('requires BOTH axes inside the alignment band', () => {
    expect(classifyCrop(2560, 1600, rect(2560, 720))).toBe('spurious')
  })

  it('treats a visible rect LARGER than coded as spurious (defensive rewrap)', () => {
    expect(classifyCrop(1920, 1080, rect(1920, 1088))).toBe('spurious')
  })
})

describe('RC_RECONNECT_LADDER_MS', () => {
  it('starts at 250 ms so a desktop transition is barely visible', () => {
    // The first retry must fire fast: a Win+L lock or M3 SYSTEM-
    // context capture handoff resolves in under a second, and a
    // 2 s first delay would leave a visible black-frame window
    // every time. Locking the first entry against accidental
    // "make it slower to be polite to the server" tweaks.
    expect(RC_RECONNECT_LADDER_MS[0]).toBe(250)
  })

  it('ends at 8 s for a real network drop', () => {
    expect(RC_RECONNECT_LADDER_MS[RC_RECONNECT_LADDER_MS.length - 1]).toBe(8000)
  })

  it('caps at 6 attempts so the operator sees a real failure within ~16 s', () => {
    expect(RC_RECONNECT_LADDER_MS.length).toBe(6)
    // Sum the ladder. Worst case (every attempt fails on its
    // delay tick) operator sees error after this many ms.
    const sum = RC_RECONNECT_LADDER_MS.reduce((a, b) => a + b, 0)
    expect(sum).toBeLessThanOrEqual(20_000)
  })

  it('is monotonically non-decreasing', () => {
    for (let i = 1; i < RC_RECONNECT_LADDER_MS.length; i++) {
      expect(RC_RECONNECT_LADDER_MS[i]).toBeGreaterThanOrEqual(
        RC_RECONNECT_LADDER_MS[i - 1],
      )
    }
  })
})

describe('parseControlInbound', () => {
  it('parses a well-formed rc:host_locked locked=true', () => {
    const r = parseControlInbound('{"t":"rc:host_locked","locked":true}')
    expect(r).toEqual({ kind: 'host_locked', locked: true })
  })

  it('parses a well-formed rc:host_locked locked=false', () => {
    const r = parseControlInbound('{"t":"rc:host_locked","locked":false}')
    expect(r).toEqual({ kind: 'host_locked', locked: false })
  })

  it('returns null for non-string input', () => {
    // Real ondatachannel messages can deliver Blob / ArrayBuffer
    // when the sender uses binary mode; our agent always sends
    // text but the type guard must not crash on the alternative.
    expect(parseControlInbound(null)).toBeNull()
    expect(parseControlInbound(123)).toBeNull()
    expect(parseControlInbound(new ArrayBuffer(8))).toBeNull()
  })

  it('returns null for non-JSON strings', () => {
    expect(parseControlInbound('not json')).toBeNull()
    expect(parseControlInbound('')).toBeNull()
    expect(parseControlInbound('{')).toBeNull()
  })

  it('returns null for JSON that is not an object', () => {
    // `JSON.parse` accepts bare values; the wire format requires
    // an envelope object so anything else is a wire-format bug
    // the older agent / future agent might emit.
    expect(parseControlInbound('null')).toBeNull()
    expect(parseControlInbound('42')).toBeNull()
    expect(parseControlInbound('"string"')).toBeNull()
    expect(parseControlInbound('[1,2,3]')).toBeNull()
  })

  it('returns null for unknown envelope types', () => {
    // Future agent versions may emit additional `t` values; older
    // browsers must skip them silently rather than crash.
    expect(parseControlInbound('{"t":"rc:cursor-shape","data":"..."}')).toBeNull()
    expect(parseControlInbound('{"t":"unknown"}')).toBeNull()
    expect(parseControlInbound('{}')).toBeNull()
  })

  it('returns null when locked is not a boolean', () => {
    // Defensive: a malformed agent that sends locked="true" or
    // locked=1 must NOT pass through as truthy. Lock state UI
    // should never be steered by stringly-typed input.
    expect(parseControlInbound('{"t":"rc:host_locked","locked":"true"}')).toBeNull()
    expect(parseControlInbound('{"t":"rc:host_locked","locked":1}')).toBeNull()
    expect(parseControlInbound('{"t":"rc:host_locked"}')).toBeNull()
  })

  it('parses a well-formed rc:desktop_changed', () => {
    // M3 A1 SYSTEM-context worker emits this after every
    // try_change_desktop Switched. Powers the secondary
    // "On Winlogon" chip.
    const r = parseControlInbound('{"t":"rc:desktop_changed","name":"Winlogon"}')
    expect(r).toEqual({ kind: 'desktop_changed', name: 'Winlogon' })
  })

  it('parses rc:desktop_changed with arbitrary desktop name', () => {
    // Default / Winlogon are the common cases but Windows can
    // present screen-saver / custom desktops too. Don't restrict.
    const r = parseControlInbound('{"t":"rc:desktop_changed","name":"Default"}')
    expect(r).toEqual({ kind: 'desktop_changed', name: 'Default' })
    const r2 = parseControlInbound('{"t":"rc:desktop_changed","name":"Screen-saver"}')
    expect(r2).toEqual({ kind: 'desktop_changed', name: 'Screen-saver' })
  })

  it('returns null when desktop_changed name is missing or wrong type', () => {
    // Defensive: stringly-typed numbers / null / missing field all
    // get rejected so we can never set currentDesktop to a
    // non-string runtime value.
    expect(parseControlInbound('{"t":"rc:desktop_changed"}')).toBeNull()
    expect(parseControlInbound('{"t":"rc:desktop_changed","name":42}')).toBeNull()
    expect(parseControlInbound('{"t":"rc:desktop_changed","name":null}')).toBeNull()
  })

  it('returns null when desktop_changed name is empty string', () => {
    // Empty name has no semantic meaning + the viewer would render
    // an empty chip, so reject at the parse layer.
    expect(parseControlInbound('{"t":"rc:desktop_changed","name":""}')).toBeNull()
  })

  // rc.23 — rc:logs-fetch.reply round-trip from the agent's
  // diagnostic log-tail handler. Browser uses this to surface ESET
  // / sync_data failures the operator can't see otherwise.
  it('parses rc:logs-fetch.reply with ok=true and lines array', () => {
    const r = parseControlInbound(
      '{"t":"rc:logs-fetch.reply","ok":true,"path":"C:\\\\Users\\\\me\\\\log","lines":["a","b","c"],"truncated":false}'
    )
    expect(r).toEqual({
      kind: 'logs_fetch_reply',
      reply: {
        ok: true,
        path: 'C:\\Users\\me\\log',
        lines: ['a', 'b', 'c'],
        truncated: false,
      },
    })
  })

  it('parses rc:logs-fetch.reply ok=false with error message', () => {
    // Agent's fetch_tail() failed path (e.g. log file rotated mid-
    // read). Browser surfaces the message in a red caption.
    const r = parseControlInbound(
      '{"t":"rc:logs-fetch.reply","ok":false,"error":"no roomler-agent.log* file"}'
    )
    expect(r).toEqual({
      kind: 'logs_fetch_reply',
      reply: { ok: false, error: 'no roomler-agent.log* file' },
    })
  })

  it('rc:logs-fetch.reply filters non-string entries from lines', () => {
    // Defensive: agent contract requires string lines but a future
    // wire-format drift shouldn't crash the browser. Filter to
    // strings before the UI binds.
    const r = parseControlInbound(
      '{"t":"rc:logs-fetch.reply","ok":true,"lines":["good",42,null,"also good"]}'
    )
    expect(r).toEqual({
      kind: 'logs_fetch_reply',
      reply: { ok: true, lines: ['good', 'also good'] },
    })
  })

  it('rc:logs-fetch.reply treats truncated as boolean only', () => {
    // Non-boolean truncated values omit the field entirely; the UI
    // renders without the "more entries omitted" hint rather than
    // showing a stringified value.
    const r = parseControlInbound(
      '{"t":"rc:logs-fetch.reply","ok":true,"truncated":"yes"}'
    )
    expect(r).toEqual({ kind: 'logs_fetch_reply', reply: { ok: true } })
  })
})

// rc.NEXT — remote app selection & launch (virtual-desktop hosts). The
// Apps menu is driven entirely by these pure parsers + wire builders, so
// they carry the wire-format invariants.
describe('parseAppsListReply', () => {
  it('parses a full list reply with windows + launchable', () => {
    const reply = parseAppsListReply({
      t: 'rc:apps.list.reply',
      id: 'a1',
      ok: true,
      supported: true,
      windows: [
        { window_id: '0x1', title: 'Terminal (main)', session: 'main', focused: true },
        { window_id: '0x2', title: 'htop', app_key: 'htop', focused: false },
      ],
      launchable: [{ key: 'bash', label: 'New bash session' }],
    })
    expect(reply.ok).toBe(true)
    expect(reply.supported).toBe(true)
    expect(reply.windows).toEqual([
      { window_id: '0x1', title: 'Terminal (main)', session: 'main', focused: true },
      { window_id: '0x2', title: 'htop', app_key: 'htop', focused: false },
    ])
    expect(reply.launchable).toEqual([{ key: 'bash', label: 'New bash session' }])
  })

  it('defaults ok/supported to false and arrays to empty when missing (version skew)', () => {
    const reply = parseAppsListReply({ t: 'rc:apps.list.reply' })
    expect(reply).toEqual({ ok: false, supported: false, windows: [], launchable: [] })
  })

  it('filters malformed window + launchable entries', () => {
    const reply = parseAppsListReply({
      ok: true,
      supported: true,
      windows: [
        { window_id: '0x1', title: 'ok', focused: false },
        { window_id: 42, title: 'bad id' },
        { title: 'no id' },
        'not-an-object',
      ],
      launchable: [{ key: 'bash', label: 'Bash' }, { key: 'x' }, { label: 'y' }],
    })
    expect(reply.windows).toEqual([{ window_id: '0x1', title: 'ok', focused: false }])
    expect(reply.launchable).toEqual([{ key: 'bash', label: 'Bash' }])
  })

  it('carries an error string when present and coerces non-boolean focused', () => {
    const reply = parseAppsListReply({
      ok: false,
      supported: true,
      error: 'wmctrl not installed',
      windows: [{ window_id: '0x1', title: 't', focused: 'yes' }],
    })
    expect(reply.error).toBe('wmctrl not installed')
    expect(reply.windows[0].focused).toBe(false)
  })
})

describe('parseAppsActionReply', () => {
  it('parses focus/launch ok replies with optional window_id', () => {
    expect(parseAppsActionReply({ ok: true })).toEqual({ ok: true })
    expect(parseAppsActionReply({ ok: true, window_id: '0xNEW' })).toEqual({
      ok: true,
      window_id: '0xNEW',
    })
  })

  it('parses error replies and coerces non-boolean ok to false', () => {
    expect(parseAppsActionReply({ ok: false, error: 'no such window' })).toEqual({
      ok: false,
      error: 'no such window',
    })
    expect(parseAppsActionReply({ ok: 'truthy' }).ok).toBe(false)
  })
})

describe('parseControlInbound — rc:apps.*', () => {
  it('routes rc:apps.list.reply with id', () => {
    const r = parseControlInbound(
      '{"t":"rc:apps.list.reply","id":"a1","ok":true,"supported":true,"windows":[],"launchable":[]}'
    )
    expect(r).toEqual({
      kind: 'apps_list_reply',
      id: 'a1',
      reply: { ok: true, supported: true, windows: [], launchable: [] },
    })
  })

  it('tolerates a null / missing id on apps replies', () => {
    const r = parseControlInbound('{"t":"rc:apps.list.reply","ok":true,"supported":false}')
    expect(r?.kind).toBe('apps_list_reply')
    expect((r as { id: string | null }).id).toBeNull()
  })

  it('routes focus + launch replies', () => {
    expect(parseControlInbound('{"t":"rc:apps.focus.reply","id":"f1","ok":true}')).toEqual({
      kind: 'apps_focus_reply',
      id: 'f1',
      reply: { ok: true },
    })
    expect(
      parseControlInbound('{"t":"rc:apps.launch.reply","id":"l1","ok":true,"window_id":"0x9"}')
    ).toEqual({
      kind: 'apps_launch_reply',
      id: 'l1',
      reply: { ok: true, window_id: '0x9' },
    })
  })

  it('returns null for an unknown rc:apps.* subtype (forward-compat)', () => {
    expect(parseControlInbound('{"t":"rc:apps.something-new"}')).toBeNull()
  })
})

describe('apps wire builders', () => {
  it('appsListWireMessage builds { t, id } and rejects empty id', () => {
    expect(appsListWireMessage('a1')).toEqual({ t: 'rc:apps.list', id: 'a1' })
    expect(appsListWireMessage('')).toBeNull()
  })

  it('appsFocusWireMessage requires id + windowId', () => {
    expect(appsFocusWireMessage('a1', '0x5')).toEqual({
      t: 'rc:apps.focus',
      id: 'a1',
      window_id: '0x5',
    })
    expect(appsFocusWireMessage('a1', '')).toBeNull()
    expect(appsFocusWireMessage('', '0x5')).toBeNull()
  })

  it('appsLaunchWireMessage requires id + appKey', () => {
    expect(appsLaunchWireMessage('a1', 'bash')).toEqual({
      t: 'rc:apps.launch',
      id: 'a1',
      app_key: 'bash',
    })
    expect(appsLaunchWireMessage('a1', '')).toBeNull()
    expect(appsLaunchWireMessage('', 'bash')).toBeNull()
  })
})

describe('nextReconnectDelayMs', () => {
  it('returns the ladder value for valid attempt indices', () => {
    expect(nextReconnectDelayMs(0)).toBe(250)
    expect(nextReconnectDelayMs(1)).toBe(500)
    expect(nextReconnectDelayMs(2)).toBe(1000)
    expect(nextReconnectDelayMs(3)).toBe(2000)
    expect(nextReconnectDelayMs(4)).toBe(4000)
    expect(nextReconnectDelayMs(5)).toBe(8000)
  })

  it('falls back to steady-state delay past the ladder (rc.23: infinite retry)', () => {
    // rc.23 — operators on AV-protected hosts need indefinite retry;
    // returning `null` past the cap surfaced "budget exhausted" in
    // the field. The 7th attempt and beyond return
    // `RC_RECONNECT_STEADY_MS` (8 s) — caller keeps retrying.
    expect(nextReconnectDelayMs(6)).toBe(8000)
    expect(nextReconnectDelayMs(100)).toBe(8000)
    expect(nextReconnectDelayMs(10_000)).toBe(8000)
  })

  it('returns the first-attempt delay on negative input (defensive)', () => {
    // Defensive: a logic bug that decremented the counter past 0
    // shouldn't strand the loop. Returns the first-attempt delay
    // (250 ms) so the loop continues. rc.23 — was `null` pre-change.
    expect(nextReconnectDelayMs(-1)).toBe(250)
  })
})

describe('nextDirPath', () => {
  // Returns null when the entry isn't a directory — the drawer's
  // dbl-click handler short-circuits before navigating.
  it('returns null for non-directory entries', () => {
    expect(
      nextDirPath({ name: 'report.pdf', is_dir: false }, 'C:\\Users', false)
    ).toBeNull()
  })

  describe('roots view', () => {
    // Roots view: drive INTO entry.name directly. Concatenating with
    // a localised "Drives" label produced bogus paths like
    // `Drives/C:\` (rc.15 field repro 2026-05-07). The fix uses an
    // explicit `isRootsView` flag.
    it('Windows drive: dbl-click "C:\\" lands at C:\\, not Drives/C:\\', () => {
      // currentDirPath comes from the agent's roots listing as
      // "Drives" — must be ignored.
      expect(
        nextDirPath({ name: 'C:\\', is_dir: true }, 'Drives', true)
      ).toBe('C:\\')
    })

    it('Unix root: dbl-click "/" lands at /, not //', () => {
      expect(
        nextDirPath({ name: '/', is_dir: true }, '/', true)
      ).toBe('/')
    })
  })

  describe('inside a real directory', () => {
    it('Windows: appends with backslash, no double-up on trailing sep', () => {
      // Trailing separator on the parent (after canonicalize) — must
      // NOT produce `C:\\dev`. This is the literal regression case
      // for `\\?\C:\` whose canonicalised form ends in `\`.
      expect(
        nextDirPath({ name: 'dev', is_dir: true }, '\\\\?\\C:\\', false)
      ).toBe('\\\\?\\C:\\dev')
      // No-trailing-sep parent → adds backslash.
      expect(
        nextDirPath({ name: 'gjovanov', is_dir: true }, 'C:\\dev', false)
      ).toBe('C:\\dev\\gjovanov')
    })

    it('Unix: appends with forward slash, no double-up on trailing sep', () => {
      expect(
        nextDirPath({ name: 'home', is_dir: true }, '/', false)
      ).toBe('/home')
      expect(
        nextDirPath({ name: 'goran', is_dir: true }, '/home', false)
      ).toBe('/home/goran')
    })

    it('detects Windows separator from drive-letter prefix', () => {
      // `C:\Users` → Windows backslash heuristic.
      expect(
        nextDirPath({ name: 'me', is_dir: true }, 'C:\\Users', false)
      ).toBe('C:\\Users\\me')
    })

    it('treats path with no backslashes + no drive letter as Unix', () => {
      expect(
        nextDirPath({ name: 'b', is_dir: true }, '/usr/local', false)
      ).toBe('/usr/local/b')
    })
  })

  describe('regression: \\\\?\\C:\\ → dev', () => {
    // Exact reproduction of the field bug fixed 2026-05-09. The
    // agent canonicalises `C:\` → `\\?\C:\`. `Path::parent()` of
    // `\\?\C:\` returns None, so a `currentParent === null` check
    // mis-classified the verbatim drive root as roots view, and
    // dbl-click `dev` shipped just `"dev"` to the agent —
    // "canonicalising dev". The explicit `isRootsView=false` here
    // is the correct call site signal: the user came in via
    // navigateTo(C:\\), not navigateTo("").
    it('produces the agent-acceptable absolute path \\\\?\\C:\\dev', () => {
      expect(
        nextDirPath({ name: 'dev', is_dir: true }, '\\\\?\\C:\\', false)
      ).toBe('\\\\?\\C:\\dev')
    })
  })
})

describe('pickAutoTransport (rc.190 HW×HW codec auto-rank)', () => {
  const base = (over: Partial<AutoTransportInputs>): AutoTransportInputs => ({
    agentTransports: [],
    agentHwEncoders: [],
    viewerAv1Hw: false,
    viewerHevcHw: false,
    viewerVp9Hw: false,
    viewerVp9Decodable: false,
    ...over,
  })

  it('NEO16→capable-viewer pair picks AV1 (HW on both ends)', () => {
    const r = pickAutoTransport(
      base({
        agentTransports: ['data-channel-vp9-444', 'data-channel-hevc', 'data-channel-av1'],
        agentHwEncoders: ['ffmpeg-hevc_nvenc', 'ffmpeg-av1_nvenc', 'libvpx-vp9-444-sw'],
        viewerAv1Hw: true,
        viewerHevcHw: true,
        viewerVp9Hw: true,
        viewerVp9Decodable: true,
      }),
    )
    expect(r.transport).toBe('data-channel-av1')
  })

  it('GEAL8N6→PC50045 pair picks HEVC (agent has NO AV1/VP9 HW encode)', () => {
    // UHD 630 + GTX 1650: hevc_nvenc is the only HW DC encoder.
    const r = pickAutoTransport(
      base({
        agentTransports: ['data-channel-vp9-444', 'data-channel-hevc'],
        agentHwEncoders: ['ffmpeg-hevc_nvenc', 'libvpx-vp9-444-sw'],
        viewerAv1Hw: true, // viewer could do AV1 — agent can't encode it
        viewerHevcHw: true,
        viewerVp9Hw: true,
        viewerVp9Decodable: true,
      }),
    )
    expect(r.transport).toBe('data-channel-hevc')
    expect(r.chromaOverride).toBeNull()
  })

  it('weak viewer (no HW HEVC) on an Intel sender lands on VP9 4:2:0 HW×HW', () => {
    const r = pickAutoTransport(
      base({
        agentTransports: ['data-channel-vp9-444', 'data-channel-hevc'],
        agentHwEncoders: ['ffmpeg-hevc_qsv', 'ffmpeg-vp9_qsv', 'libvpx-vp9-444-sw'],
        viewerHevcHw: false, // HEVC would be SW-decoded here — skip it
        viewerVp9Hw: true,
        viewerVp9Decodable: true,
      }),
    )
    expect(r.transport).toBe('data-channel-vp9-444')
    expect(r.chromaOverride).toBe('yuv420')
  })

  it('no HW×HW pair at all → VP9 SW-encode fallback (agent caps it ≤1920)', () => {
    const r = pickAutoTransport(
      base({
        agentTransports: ['data-channel-vp9-444'],
        agentHwEncoders: ['libvpx-vp9-444-sw'],
        viewerVp9Hw: false,
        viewerVp9Decodable: true,
      }),
    )
    expect(r.transport).toBe('data-channel-vp9-444')
    expect(r.chromaOverride).toBe('yuv420')
  })

  it('nothing decodable / nothing advertised → webrtc (null)', () => {
    expect(pickAutoTransport(base({})).transport).toBeNull()
  })

  it('derives transports from hw_encoders for pre-transports agent rows', () => {
    // Older DB rows lack `transports` (skip_serializing_if empty) — the
    // hw_encoders labels alone must still light up the HEVC path.
    const r = pickAutoTransport(
      base({
        agentHwEncoders: ['ffmpeg-hevc_nvenc'],
        viewerHevcHw: true,
      }),
    )
    expect(r.transport).toBe('data-channel-hevc')
  })
})

describe('displayMatchWireMessage (rc.191)', () => {
  it('sends rounded dims for an enable request', () => {
    expect(displayMatchWireMessage({ width: 1672.4, height: 818.6 })).toEqual({
      t: 'rc:display-match',
      width: 1672,
      height: 819,
    })
  })

  it('null / non-finite dims become a restore request', () => {
    expect(displayMatchWireMessage(null)).toEqual({ t: 'rc:display-match', enable: false })
    expect(displayMatchWireMessage({ width: NaN, height: 800 })).toEqual({
      t: 'rc:display-match',
      enable: false,
    })
  })
})

describe('AV1_CODEC_STRING (rc.190)', () => {
  it('is Main profile, level 5.1, Main tier, 8-bit — covers 4K@60', () => {
    // The declared level is a MAX (HEVC L3.1 lesson: too low a level
    // hard-rejects streams above it); 13 = 5.1.
    expect(AV1_CODEC_STRING).toBe('av01.0.13M.08')
  })
})

describe('decodeStatWireMessage (rc.188 viewer-rate feedback)', () => {
  it('rounds + clamps the reported fps and carries the struggling bit', () => {
    expect(decodeStatWireMessage(58.7, true)).toEqual({
      t: 'rc:decodestat',
      fps: 59,
      struggling: true,
    })
    expect(decodeStatWireMessage(30, false)).toEqual({
      t: 'rc:decodestat',
      fps: 30,
      struggling: false,
    })
  })

  it('coerces non-finite / negative fps to 0 (a clean "no useful number")', () => {
    expect(decodeStatWireMessage(NaN, false).fps).toBe(0)
    expect(decodeStatWireMessage(-5, true).fps).toBe(0)
    expect(decodeStatWireMessage(Infinity, false).fps).toBe(0)
  })

  it('caps absurd fps at 240 so the packed 16-bit agent field never overflows', () => {
    expect(decodeStatWireMessage(100000, true).fps).toBe(240)
  })
})

describe('priorityWireMessage (rc.199 Priority dial)', () => {
  it('builds the rc:priority envelope for each dial', () => {
    expect(priorityWireMessage('balanced')).toEqual({ t: 'rc:priority', mode: 'balanced' })
    expect(priorityWireMessage('sharper')).toEqual({ t: 'rc:priority', mode: 'sharper' })
    expect(priorityWireMessage('smoother')).toEqual({ t: 'rc:priority', mode: 'smoother' })
  })
})

describe('codecChoiceToSettings (rc.199 unified Codec picker)', () => {
  it('maps every choice to a full transport/chroma/codec/render tuple', () => {
    expect(codecChoiceToSettings('auto')).toEqual({
      videoTransport: 'auto',
      chroma: 'auto',
      preferredCodec: null,
      renderPath: 'webcodecs',
    })
    expect(codecChoiceToSettings('av1')).toEqual({
      videoTransport: 'data-channel-av1',
      chroma: 'auto',
      preferredCodec: null,
      renderPath: 'webcodecs',
    })
    expect(codecChoiceToSettings('hevc')).toEqual({
      videoTransport: 'data-channel-hevc',
      chroma: 'auto',
      preferredCodec: null,
      renderPath: 'webcodecs',
    })
    // The two VP9 choices share a transport and differ ONLY in chroma —
    // 4:4:4 = crisp text, 4:2:0 = efficient.
    expect(codecChoiceToSettings('vp9-444')).toEqual({
      videoTransport: 'data-channel-vp9-444',
      chroma: 'yuv444',
      preferredCodec: null,
      renderPath: 'webcodecs',
    })
    expect(codecChoiceToSettings('vp9-420')).toEqual({
      videoTransport: 'data-channel-vp9-444',
      chroma: 'yuv420',
      preferredCodec: null,
      renderPath: 'webcodecs',
    })
    // H.264 = the "max compatibility" choice: revives the RTP track +
    // preferredCodec AND uses the plain <video> render path (not WebCodecs).
    expect(codecChoiceToSettings('h264')).toEqual({
      videoTransport: 'webrtc',
      chroma: 'auto',
      preferredCodec: 'h264',
      renderPath: 'video',
    })
  })
})

describe('settingsToCodecChoice (rc.199 reverse map)', () => {
  it('derives the picker value from the stored transport + chroma', () => {
    expect(settingsToCodecChoice('auto', 'auto')).toBe('auto')
    expect(settingsToCodecChoice('data-channel-av1', 'auto')).toBe('av1')
    expect(settingsToCodecChoice('data-channel-hevc', 'auto')).toBe('hevc')
    expect(settingsToCodecChoice('data-channel-vp9-444', 'yuv444')).toBe('vp9-444')
    expect(settingsToCodecChoice('data-channel-vp9-444', 'yuv420')).toBe('vp9-420')
    // Legacy vp9-444 sessions stored chroma 'auto' → read as the efficient
    // 4:2:0 choice (never silently promoted to the heavier 4:4:4).
    expect(settingsToCodecChoice('data-channel-vp9-444', 'auto')).toBe('vp9-420')
    expect(settingsToCodecChoice('webrtc', 'auto')).toBe('h264')
  })

  it('round-trips every choice through settings and back', () => {
    const choices: RcCodecChoice[] = ['auto', 'av1', 'hevc', 'vp9-444', 'vp9-420', 'h264']
    for (const c of choices) {
      const s = codecChoiceToSettings(c)
      expect(settingsToCodecChoice(s.videoTransport, s.chroma)).toBe(c)
    }
  })
})

describe('parseControlInbound — rc:video-info native dims (rc.199)', () => {
  it('parses native_w/native_h when the agent reports them', () => {
    const parsed = parseControlInbound(
      '{"t":"rc:video-info","codec":"vp9","encoder":"libvpx","hardware":false,"chroma":"yuv444","transport":"relay","native_w":2560,"native_h":1600}',
    )
    expect(parsed).toEqual({
      kind: 'video_info',
      info: {
        codec: 'vp9',
        encoder: 'libvpx',
        hardware: false,
        chroma: 'yuv444',
        transport: 'relay',
        native_w: 2560,
        native_h: 1600,
      },
    })
  })

  it('defaults native dims to 0 for older agents that omit them (back-compat)', () => {
    const parsed = parseControlInbound(
      '{"t":"rc:video-info","codec":"h265","encoder":"hevc_nvenc","hardware":true,"chroma":"yuv420","transport":"direct"}',
    )
    expect(parsed?.kind).toBe('video_info')
    if (parsed?.kind === 'video_info') {
      expect(parsed.info.native_w).toBe(0)
      expect(parsed.info.native_h).toBe(0)
    }
  })
})

describe('parseLocalRelayDescriptor (Phase 2 loopback-TURN corp-relay)', () => {
  it('accepts a well-formed descriptor', () => {
    expect(
      parseLocalRelayDescriptor({
        turn_port: 47990,
        overlay_ip: '100.64.0.5',
        username: '1700000600:uid',
        credential: 'abc',
      }),
    ).toEqual({
      turn_port: 47990,
      overlay_ip: '100.64.0.5',
      username: '1700000600:uid',
      credential: 'abc',
    })
  })

  it('rejects malformed / missing / out-of-range blobs (untrusted loopback JSON)', () => {
    const base = { turn_port: 47990, overlay_ip: 'x', username: 'u', credential: 'c' }
    expect(parseLocalRelayDescriptor(null)).toBeNull()
    expect(parseLocalRelayDescriptor('nope')).toBeNull()
    expect(parseLocalRelayDescriptor({})).toBeNull()
    expect(parseLocalRelayDescriptor({ ...base, turn_port: 0 })).toBeNull()
    expect(parseLocalRelayDescriptor({ ...base, turn_port: 70000 })).toBeNull()
    expect(parseLocalRelayDescriptor({ ...base, turn_port: 1.5 })).toBeNull()
    expect(parseLocalRelayDescriptor({ ...base, overlay_ip: '' })).toBeNull()
    expect(parseLocalRelayDescriptor({ ...base, username: 5 })).toBeNull()
    expect(parseLocalRelayDescriptor({ ...base, credential: undefined })).toBeNull()
  })
})

describe('localRelayIceServer (Phase 2)', () => {
  it('builds the loopback turn: ICE server from a descriptor', () => {
    expect(
      localRelayIceServer({
        turn_port: 47990,
        overlay_ip: '100.64.0.5',
        username: '1700000600:uid',
        credential: 'abc',
      }),
    ).toEqual({
      urls: ['turn:127.0.0.1:47990'],
      username: '1700000600:uid',
      credential: 'abc',
    })
  })

  it('always dials loopback (never the overlay IP — that is the remote agent entry)', () => {
    const s = localRelayIceServer({
      turn_port: 12345,
      overlay_ip: '100.64.9.9',
      username: 'u',
      credential: 'c',
    })
    expect(s.urls[0]).toBe('turn:127.0.0.1:12345')
    expect(s.urls[0]).not.toContain('100.64')
  })
})
