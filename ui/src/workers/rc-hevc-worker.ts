/// <reference lib="webworker" />

/**
 * rc.78 — HEVC decoder worker for the Option B HEVC-over-DataChannel
 * transport. Sibling to `rc-vp9-444-worker.ts`; same wire format, same
 * 13-byte length-prefix header, same `OffscreenCanvas` paint path.
 * Only difference: codec string is `hev1.1.6.L153.B0` (HEVC Main
 * profile, Level 5.1, no decoder description). The pre-flight WebCodecs
 * spike (2026-05-26) confirmed Chrome + Edge accept Annex-B no-
 * description HEVC at L3.1; rc.94 bumped the level to 5.1 because the
 * field SystemContext host captures 1920×1200, which exceeds L3.1's
 * ~1280×720 ceiling and rendered black (see DEFAULT_HEVC_CODEC note).
 *
 * Source agent: `agents/roomler-agent/src/encode/ffmpeg/encoder.rs`
 * via `peer.rs::media_pump_hevc_dc`. Cascade: hevc_nvenc → hevc_qsv
 * → hevc_amf. Gate 0 smoke (2026-05-29) validated both NVENC on RTX
 * 5090 Blackwell AND QSV on Iris Xe Tiger Lake — the two MF-broken
 * platforms the entire Option B plan was built to bypass.
 *
 * Wire format (matches `frame_video_bytes` in agent peer.rs:1502):
 *
 *   struct Frame {
 *     u32  size_le;          // payload length, little-endian
 *     u8   flags;            // bit 0: keyframe
 *     u64  timestamp_us;     // monotonic capture timestamp
 *     [u8] payload;          // HEVC Annex-B (4-byte start codes,
 *                            // AUD → VPS → SPS → PPS → IDR on keyframes)
 *   }
 *
 * Header is 13 bytes. SCTP is reliable+ordered so a single frame may
 * arrive across multiple `onmessage` chunks; the assembler in this
 * worker concatenates by tracking how many bytes of the declared
 * payload size remain.
 */

type InitCanvasMessage = {
  type: 'init-canvas'
  canvas: OffscreenCanvas
  /** Optional codec override. Default is HEVC Main profile, L3.1
   *  (1080p) which covers Gate 0's test resolutions. Bump to
   *  `hev1.1.6.L120.90` (L4.0) for 4K or `hev1.1.6.L150.90` (L5.0)
   *  for 4K60. Tests may override. */
  codec?: string
}
type ChunkMessage = { type: 'chunk'; bytes: ArrayBuffer }
type CloseMessage = { type: 'close' }
type IncomingMessage = InitCanvasMessage | ChunkMessage | CloseMessage

const HEADER_BYTES = 13
const FLAG_KEYFRAME = 0x01
/** Default codec — HEVC Main, Level **5.1**, no decoder description.
 *  rc.94 — bumped from L3.1 (`L93`). L3.1 maxes at 983,040 luma
 *  samples (~1280×720); the field SystemContext host (PC50054)
 *  captures 1920×1200 = 2,304,000 samples, which exceeds even L4.1
 *  (2,228,224) — so Chromium's HW HEVC decoder rejected the stream
 *  and rendered a BLACK screen. The pre-flight spike only validated a
 *  low-res sample at L3.1, so it never caught this. L5.1 (`L153`,
 *  8,912,896 samples, up to 4K@64) covers every realistic desktop
 *  resolution and is HW-decodable on Iris Xe / RTX 5090 (the Gate 0
 *  hosts). The codec-string level is a declared MAX; a smaller stream
 *  decodes fine under it. Tests can override via the `codec` field on
 *  `init-canvas`. */
const DEFAULT_HEVC_CODEC = 'hev1.1.6.L153.B0'
let activeCodec: string = DEFAULT_HEVC_CODEC

const workerScope = self as unknown as {
  onmessage: ((ev: MessageEvent<IncomingMessage>) => void) | null
  postMessage: (msg: unknown) => void
}

let canvas: OffscreenCanvas | null = null
let ctx: OffscreenCanvasRenderingContext2D | null = null
let decoder: VideoDecoder | null = null
let configured = false
let framesDecoded = 0
let framesReceived = 0

/** Rolling-window stats for the HUD, matching the VP9-444 worker so
 *  the composable can consume both with the same message shape. */
let statsBytesInWindow = 0
let statsBytesTotal = 0
let statsFramesInWindow = 0
let statsLastWidth = 0
let statsLastHeight = 0
let statsLastEmitMs: number = (typeof performance !== 'undefined' ? performance.now() : Date.now())
const STATS_EMIT_INTERVAL_MS = 1000

function maybeEmitStats(): void {
  const now = (typeof performance !== 'undefined' ? performance.now() : Date.now())
  const elapsed = now - statsLastEmitMs
  if (elapsed < STATS_EMIT_INTERVAL_MS) return
  const elapsedSec = elapsed / 1000
  const bitrateBps = Math.round((statsBytesInWindow * 8) / elapsedSec)
  const fps = Math.round(statsFramesInWindow / elapsedSec)
  workerScope.postMessage({
    type: 'stats',
    bitrateBps,
    fps,
    width: statsLastWidth,
    height: statsLastHeight,
    framesDecodedTotal: framesDecoded,
    bytesReceivedTotal: statsBytesTotal,
  })
  statsBytesInWindow = 0
  statsFramesInWindow = 0
  statsLastEmitMs = now
}

const assembler = {
  headerBuf: new Uint8Array(HEADER_BYTES),
  headerHave: 0,
  payloadBuf: null as Uint8Array | null,
  payloadHave: 0,
  pendingPayloadSize: 0,
  pendingFlags: 0,
  pendingTimestampUs: 0n,
}

workerScope.onmessage = (e) => {
  const msg = e.data
  if (!msg) return
  if (msg.type === 'init-canvas') {
    canvas = msg.canvas
    ctx = canvas.getContext('2d')
    if (typeof msg.codec === 'string' && msg.codec.length > 0) {
      activeCodec = msg.codec
    }
    initDecoder()
  } else if (msg.type === 'chunk') {
    framesReceived++
    const u8 = new Uint8Array(msg.bytes)
    statsBytesInWindow += u8.byteLength
    statsBytesTotal += u8.byteLength
    consumeBytes(u8)
    maybeEmitStats()
  } else if (msg.type === 'close') {
    teardown()
  }
}

function initDecoder() {
  if (decoder) return
  decoder = new VideoDecoder({
    output: (frame) => {
      framesDecoded++
      statsFramesInWindow++
      // rc.100 — Chrome's NVDEC HEVC decode reports a SHRUNKEN
      // displayWidth/Height for our hevc_nvenc stream (field GORAN-XMG-NEO16,
      // RTX 5090: agent encodes 2560×1600 — proven by the pump heartbeat —
      // but `displayWidth/Height` come out 1280×720, and the aspect even
      // changes 16:10→16:9, so it is NOT a clean SAR). Drive the canvas +
      // the reported intrinsic dims from the decoded buffer's CODED size,
      // which is the true resolution — this restores correct geometry
      // (canvas aspect AND the controller's mouse-normalisation, both of
      // which were broken by the 16:9 displayWidth). `displayWidth`/
      // `visibleRect` are forwarded in the one-shot first-frame message
      // purely as a field diagnostic so we can confirm the coded↔display
      // gap without another round-trip.
      const codedW = frame.codedWidth || frame.displayWidth
      const codedH = frame.codedHeight || frame.displayHeight
      statsLastWidth = codedW
      statsLastHeight = codedH
      paintFrame(frame, codedW, codedH)
      if (framesDecoded === 1) {
        const vr = frame.visibleRect
        workerScope.postMessage({
          type: 'first-frame',
          width: codedW,
          height: codedH,
          coded: { w: frame.codedWidth, h: frame.codedHeight },
          display: { w: frame.displayWidth, h: frame.displayHeight },
          visible: vr ? { x: vr.x, y: vr.y, w: vr.width, h: vr.height } : null,
        })
      } else {
        workerScope.postMessage({ type: 'frame-decoded', count: framesDecoded })
      }
      maybeEmitStats()
    },
    error: (err) => {
      workerScope.postMessage({
        type: 'decoder-error',
        error: extractErrorMessage(err),
      })
    },
  })
  // Configure. HEVC WebCodecs support is HW-only in Chromium today —
  // there's no in-tree SW HEVC decoder. If the platform's OS HEVC
  // decoder isn't available (Linux Chromium typically, corporate
  // policy disabled, very old hardware), `isConfigSupported` returns
  // false and the composable falls back to VP9-444-DC before this
  // worker is even instantiated. By the time we get here, the
  // platform claimed support — but mid-session HW driver hiccup can
  // still rip the rug; the `error` callback above surfaces it to the
  // main thread for re-negotiation.
  //
  // hardwareAcceleration='no-preference' lets Chromium pick: HW
  // where it exists (Win11 + RTX 5090 / Iris Xe is what Gate 0
  // validated), no fallback otherwise (HEVC HAS no SW fallback in
  // WebCodecs, unlike VP9). Same setting as the VP9-444 worker for
  // consistency.
  try {
    decoder.configure({
      codec: activeCodec,
      optimizeForLatency: true,
      hardwareAcceleration: 'no-preference',
    } as VideoDecoderConfig)
    configured = true
    workerScope.postMessage({
      type: 'decoder-configured',
      codec: activeCodec,
    })
  } catch (err) {
    workerScope.postMessage({
      type: 'decoder-configure-error',
      codec: activeCodec,
      error: extractErrorMessage(err),
    })
  }
}

function consumeBytes(bytes: Uint8Array): void {
  let cursor = 0
  while (cursor < bytes.length) {
    if (assembler.payloadBuf === null) {
      const need = HEADER_BYTES - assembler.headerHave
      const take = Math.min(need, bytes.length - cursor)
      assembler.headerBuf.set(
        bytes.subarray(cursor, cursor + take),
        assembler.headerHave,
      )
      assembler.headerHave += take
      cursor += take
      if (assembler.headerHave === HEADER_BYTES) {
        const view = new DataView(
          assembler.headerBuf.buffer,
          assembler.headerBuf.byteOffset,
          HEADER_BYTES,
        )
        const size = view.getUint32(0, true)
        const flags = view.getUint8(4)
        const ts_lo = view.getUint32(5, true)
        const ts_hi = view.getUint32(9, true)
        const ts_us = (BigInt(ts_hi) << 32n) | BigInt(ts_lo)
        if (size === 0 || size > 16 * 1024 * 1024) {
          // Out-of-spec; drop and resync. 16 MB cap covers 4K HEVC
          // IDRs (typically 0.5-2 MB at our bitrates) with headroom.
          workerScope.postMessage({
            type: 'frame-rejected',
            reason: 'implausible-size',
            size,
          })
          assembler.headerHave = 0
          continue
        }
        assembler.payloadBuf = new Uint8Array(size)
        assembler.payloadHave = 0
        assembler.pendingPayloadSize = size
        assembler.pendingFlags = flags
        assembler.pendingTimestampUs = ts_us
      }
    } else {
      const need = assembler.pendingPayloadSize - assembler.payloadHave
      const take = Math.min(need, bytes.length - cursor)
      assembler.payloadBuf.set(
        bytes.subarray(cursor, cursor + take),
        assembler.payloadHave,
      )
      assembler.payloadHave += take
      cursor += take
      if (assembler.payloadHave === assembler.pendingPayloadSize) {
        emitFrame()
        assembler.headerHave = 0
        assembler.payloadBuf = null
        assembler.payloadHave = 0
      }
    }
  }
}

function emitFrame(): void {
  if (!decoder || !configured) return
  const payload = assembler.payloadBuf
  if (!payload) return
  const isKey = (assembler.pendingFlags & FLAG_KEYFRAME) !== 0
  const ts = Number(assembler.pendingTimestampUs)
  try {
    const chunk = new EncodedVideoChunk({
      type: isKey ? 'key' : 'delta',
      timestamp: ts,
      data: payload,
    })
    decoder.decode(chunk)
  } catch (err) {
    workerScope.postMessage({
      type: 'decode-error',
      error: extractErrorMessage(err),
    })
  }
}

function paintFrame(frame: VideoFrame, w: number, h: number): void {
  if (!canvas || !ctx) {
    frame.close()
    return
  }
  try {
    if (canvas.width !== w) canvas.width = w
    if (canvas.height !== h) canvas.height = h
    // rc.100 — pass the explicit dest rect. The bare `drawImage(frame, 0, 0)`
    // uses the frame's `displayWidth/Height` as its natural size, which is
    // exactly the shrunken value we're routing around; an explicit dest of
    // the coded size keeps the output at full resolution.
    ctx.drawImage(frame, 0, 0, w, h)
  } catch {
    /* canvas lost mid-teardown */
  } finally {
    frame.close()
  }
}

function teardown(): void {
  try {
    decoder?.close()
  } catch {
    /* idempotent */
  }
  decoder = null
  configured = false
  canvas = null
  ctx = null
  framesDecoded = 0
  framesReceived = 0
  assembler.headerHave = 0
  assembler.payloadBuf = null
  assembler.payloadHave = 0
}

function extractErrorMessage(err: unknown): string {
  if (err instanceof Error) return err.message
  try {
    return String(err)
  } catch {
    return 'unknown'
  }
}

/** Exported for vitest — pure function that parses the 13-byte
 *  frame header. Same wire format as the VP9-444 path. */
export function parseFrameHeader(buf: Uint8Array): {
  payloadSize: number
  flags: number
  timestampUs: bigint
} | null {
  if (buf.length < HEADER_BYTES) return null
  const view = new DataView(buf.buffer, buf.byteOffset, HEADER_BYTES)
  const size = view.getUint32(0, true)
  const flags = view.getUint8(4)
  const ts_lo = view.getUint32(5, true)
  const ts_hi = view.getUint32(9, true)
  const ts_us = (BigInt(ts_hi) << 32n) | BigInt(ts_lo)
  return { payloadSize: size, flags, timestampUs: ts_us }
}

export function isKeyframe(flags: number): boolean {
  return (flags & FLAG_KEYFRAME) !== 0
}
