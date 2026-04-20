import { describe, it, expect, beforeEach, afterAll } from 'vitest'

// Pure helpers exported for testing. We can't import the full composable
// here without mocking the WS store; the helpers below are self-contained
// pure functions and are what actually determine the wire format, so they
// carry the important invariants.
import {
  browserButton,
  kbdCodeToHid,
  letterboxedNormalise,
  extractStatsSnapshot,
  inspectBrowserVideoCodecs,
  base64ToBytes,
} from '@/composables/useRemoteControl'

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
