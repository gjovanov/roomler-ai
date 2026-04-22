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
  shortCodecFromReceiver,
} from '@/composables/useRemoteControl'
import { codecMimeForShort } from '@/workers/rc-webcodecs-worker'

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
    expect(codecMimeForShort('h265')).toBe('hev1.1.6.L93.B0')
    expect(codecMimeForShort('hevc')).toBe('hev1.1.6.L93.B0')
    expect(codecMimeForShort('av1')).toBe('av01.0.08M.08')
    expect(codecMimeForShort('vp9')).toBe('vp09.00.10.08')
    expect(codecMimeForShort('vp8')).toBe('vp8')
  })

  it('is case-insensitive', () => {
    expect(codecMimeForShort('H264')).toBe('avc1.42E01F')
    expect(codecMimeForShort('HEVC')).toBe('hev1.1.6.L93.B0')
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
