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
// rc.103 — leading-delta gate. The HW HEVC decoder rejects a delta as
// its FIRST input after configure()/flush() ("A key frame is required").
// The agent's FFmpeg QSV/NVENC encoder is ASYNC (pipeline depth): at the
// instant the DC reaches Open, the packet draining is a buffered delta
// from earlier in the encoder queue — it ships ahead of the freshly-forced
// IDR (rc.97 force-kf-on-open), so the browser's first ASSEMBLED frame is
// sometimes a delta. Field LAPTOP-P2TU89GB (hevc_qsv) hit exactly this:
// "hevc DC opened" → "decode-error: A key frame is required" → permanent
// <video> fallback. The libvpx VP9-444 path never hit it because libvpx is
// synchronous (request_keyframe → the very next packet IS the IDR). Drop
// leading deltas until the first keyframe; the IDR is only a handful of
// frames behind them in the async queue, so the wait is a few ms.
let sawKeyframe = false
let framesSkippedAwaitingKey = 0

// rc.130 — decode-backlog shed. With `optimizeForLatency` the decoder keeps
// its input queue at 0–1 when it's keeping up; a sustained queue above this
// means it has fallen behind (slow client / 4K on a weak iGPU / throttled
// background tab) and the stream latency is growing monotonically. When that
// happens we drop incoming DELTAS and ask the agent for a fresh keyframe to
// resync — never dropping a key frame (it's the resync point). Without this
// the queue never drains and "typing appears seconds later" is permanent.
const MAX_DECODE_QUEUE = 2
let framesDroppedBacklog = 0
let lastKeyframeReqMs = 0

/** Ask the agent (via the composable → `control` DC) to force an IDR so the
 *  decoder can resync after a backlog drop. Debounced so a sustained deficit
 *  can't spam keyframe requests (the largest frames) onto a congested link. */
function requestKeyframeResync(): void {
  const now = typeof performance !== 'undefined' ? performance.now() : Date.now()
  if (now - lastKeyframeReqMs < 250) return
  lastKeyframeReqMs = now
  workerScope.postMessage({ type: 'request-keyframe' })
}

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
    decodeQueueSize: decoder?.decodeQueueSize ?? 0,
    framesDroppedBacklog,
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
      // rc.100/rc.102 — Chrome's NVDEC HEVC decode mis-reports the picture
      // geometry for our hevc_nvenc stream (field GORAN-XMG-NEO16, RTX 5090):
      // the agent encodes a FULL 2560×1600 desktop (proven by the FFmpeg DC
      // pump heartbeat: w=2560 h=1600 enc=hevc_nvenc, ~3 MB/window), but the
      // decoded VideoFrame carries a SPURIOUS conformance window —
      // codedWidth/Height = 2560×1600 yet visibleRect/displayWidth = 1280×720
      // at offset (0,0). `drawImage` honours visibleRect, so we were painting
      // only the top-left region. The coded pixels ARE the whole desktop, so
      // re-wrap the frame with visibleRect = the full coded rect and render
      // THAT. Intel hevc_qsv is unaffected (visibleRect already == coded), so
      // the re-wrap is a no-op there. (rc.100 first moved to codedWidth for the
      // reported intrinsic; rc.102 also overrides the crop drawImage honours.)
      const codedW = frame.codedWidth || frame.displayWidth
      const codedH = frame.codedHeight || frame.displayHeight
      const vr = frame.visibleRect
      let render: VideoFrame = frame
      let rewrapped = false
      if (
        frame.codedWidth > 0 &&
        frame.codedHeight > 0 &&
        vr &&
        (vr.width !== frame.codedWidth || vr.height !== frame.codedHeight)
      ) {
        try {
          render = new VideoFrame(frame, {
            visibleRect: { x: 0, y: 0, width: frame.codedWidth, height: frame.codedHeight },
          })
          rewrapped = true
        } catch {
          render = frame // re-wrap rejected → fall back to the cropped frame
        }
      }
      // Capture the one-shot diagnostic BEFORE paintFrame() calls close()
      // (a closed VideoFrame reports 0/null — rc.100 logged {0,0} this way).
      let firstFrameMsg: Record<string, unknown> | null = null
      if (framesDecoded === 1) {
        firstFrameMsg = {
          type: 'first-frame',
          width: codedW,
          height: codedH,
          coded: { w: frame.codedWidth, h: frame.codedHeight },
          display: { w: frame.displayWidth, h: frame.displayHeight },
          visible: vr ? { x: vr.x, y: vr.y, w: vr.width, h: vr.height } : null,
          rewrapped,
        }
      }
      statsLastWidth = codedW
      statsLastHeight = codedH
      paintFrame(render, codedW, codedH)
      if (rewrapped) frame.close() // paintFrame closed `render`; close the original too
      if (firstFrameMsg) {
        workerScope.postMessage(firstFrameMsg)
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
  // rc.103 — gate on the first keyframe (see `sawKeyframe` note above).
  // Feeding a leading delta to the HW decoder throws "A key frame is
  // required" and the composable then tears the whole HEVC path down to
  // <video>. Drop deltas until the IDR (right behind them) arrives.
  if (!shouldDecodeFrame(sawKeyframe, isKey)) {
    framesSkippedAwaitingKey++
    // One-shot + periodic diagnostic so the field log shows the gate
    // engaged (and how many deltas it ate) without flooding.
    if (framesSkippedAwaitingKey === 1 || framesSkippedAwaitingKey % 60 === 0) {
      workerScope.postMessage({
        type: 'awaiting-keyframe',
        dropped: framesSkippedAwaitingKey,
      })
    }
    return
  }
  if (isKey && !sawKeyframe) {
    sawKeyframe = true
    workerScope.postMessage({
      type: 'keyframe-acquired',
      droppedBefore: framesSkippedAwaitingKey,
    })
  }
  // rc.130 — backlog shed. If the decoder has fallen behind, drop this delta
  // and re-arm the keyframe gate so we keep dropping deltas (which would
  // otherwise decode-error against the missing reference and tear the path
  // down to <video>) until the resync IDR lands.
  if (!isKey && decoder.decodeQueueSize > MAX_DECODE_QUEUE) {
    framesDroppedBacklog++
    sawKeyframe = false
    requestKeyframeResync()
    if (framesDroppedBacklog === 1 || framesDroppedBacklog % 60 === 0) {
      workerScope.postMessage({
        type: 'backlog-drop',
        dropped: framesDroppedBacklog,
        queue: decoder.decodeQueueSize,
      })
    }
    return
  }
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
  // rc.103 — re-arm the leading-delta gate so a fresh decoder (re-connect
  // / re-negotiation) again waits for an IDR before its first decode().
  sawKeyframe = false
  framesSkippedAwaitingKey = 0
  framesDroppedBacklog = 0
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

/** rc.103 — leading-delta gate decision, pure for vitest. Returns whether
 *  a frame should be fed to `decoder.decode()`: a delta is only decodable
 *  once a keyframe has already been seen; a keyframe is always decodable.
 *  This is the guard that stops the HW HEVC decoder throwing "A key frame
 *  is required after configure() or flush()" on a leading delta — the
 *  field LAPTOP-P2TU89GB hevc_qsv failure (async-encoder DC-open race). */
export function shouldDecodeFrame(hasSeenKeyframe: boolean, isKey: boolean): boolean {
  return hasSeenKeyframe || isKey
}
