/// <reference lib="webworker" />

/**
 * Phase Y.2 — VP9 4:4:4 decoder worker for the DataChannel-bypass
 * video transport. See `docs/vp9-444-plan.md`.
 *
 * Receives length-prefixed encoded VP9 frames from the main thread
 * (which forwards them off `RTCDataChannel.onmessage`), reassembles
 * them, feeds `VideoDecoder({codec:'vp09.01.10.08'})` (profile 1,
 * 8-bit 4:4:4), and paints the resulting `VideoFrame`s to an
 * `OffscreenCanvas`. No `RTCRtpScriptTransform` involvement — this
 * worker is independent of the broken-in-Chrome 131 RTP transform
 * path.
 *
 * Wire format (matches `agents/roomler-agent/src/encode/libvpx.rs`):
 *
 *   struct Frame {
 *     u32  size_le;          // payload length, little-endian
 *     u8   flags;            // bit 0: keyframe
 *     u64  timestamp_us;     // monotonic capture timestamp
 *     [u8] payload;          // raw VP9 frame
 *   }
 *
 * Header is 13 bytes. SCTP is reliable+ordered so a single frame
 * may arrive across multiple `onmessage` chunks; the assembler in
 * this worker concatenates by tracking how many bytes of the
 * declared payload size remain.
 */

type InitCanvasMessage = {
  type: 'init-canvas'
  canvas: OffscreenCanvas
  /** Optional codec override. Production uses VP9 profile 1
   *  (vp09.01.10.08, 4:4:4 8-bit) — the default below — but the
   *  e2e harness lacks a 4:4:4 *encoder* in current Chromium
   *  WebCodecs, so it overrides with VP9 profile 0 (vp09.00.10.08,
   *  4:2:0) just to exercise the wire+DC+decoder mechanics. */
  codec?: string
}
type ChunkMessage = { type: 'chunk'; bytes: ArrayBuffer }
type CloseMessage = { type: 'close' }
type IncomingMessage = InitCanvasMessage | ChunkMessage | CloseMessage

const HEADER_BYTES = 13
const FLAG_KEYFRAME = 0x01
/** Default codec — VP9 profile 1 (4:4:4 8-bit). Browsers' WebCodecs
 *  decoders use libvpx and accept this without quibble. Tests can
 *  override via the `codec` field on `init-canvas`. */
const DEFAULT_VP9_CODEC = 'vp09.01.10.08'
let activeCodec: string = DEFAULT_VP9_CODEC

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
// rc.103 — leading-delta gate, mirrored from rc-hevc-worker. The
// VideoDecoder rejects a delta as its FIRST input ("A key frame is
// required after configure() or flush()"). The libvpx pump that feeds
// this worker is synchronous (the first emitted packet is already the
// IDR) so the gate normally passes immediately — but the FFmpeg vp9_qsv
// path is async (pipeline depth) and can drain a buffered delta ahead of
// the DC-open IDR, exactly like the HEVC failure on LAPTOP-P2TU89GB. Drop
// leading deltas until the first keyframe so neither feeder can wedge the
// decode path into a permanent <video> fallback.
let sawKeyframe = false
let framesSkippedAwaitingKey = 0

// rc.130 — decode-backlog shed (see the HEVC worker for the rationale). Drop
// deltas + ask the agent for a resync IDR when the decoder's input queue
// grows, so a sustained decode deficit can't make latency grow without bound.
const MAX_DECODE_QUEUE = 2
let framesDroppedBacklog = 0
let lastKeyframeReqMs = 0

/** Debounced keyframe-resync request after a backlog drop. */
function requestKeyframeResync(): void {
  const now = typeof performance !== 'undefined' ? performance.now() : Date.now()
  if (now - lastKeyframeReqMs < 250) return
  lastKeyframeReqMs = now
  workerScope.postMessage({ type: 'request-keyframe' })
}

/** Rolling-window stats for the HUD. Computed in the worker so the
 *  main thread doesn't need to touch the DC traffic. Bitrate is
 *  delivered-bytes/sec at the SCTP-receive boundary (post-network,
 *  pre-decode). Latest VideoFrame dims are captured in the decoder's
 *  output callback. */
let statsBytesInWindow = 0
let statsBytesTotal = 0
let statsFramesInWindow = 0
let statsLastWidth = 0
let statsLastHeight = 0
// rc.187 — last frame dims we reported to the composable. Re-posted whenever
// the decoded size changes (the agent's viewer-adaptive resolution downscales
// mid-session), so the cursor mapping's `mediaIntrinsic` never goes stale.
let lastPostedW = 0
let lastPostedH = 0
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

/** Frame assembler state — rolling buffer plus the size we're trying
 *  to fill on the current frame. Fragments concatenate until
 *  `pendingPayloadSize` is satisfied; then we emit the chunk. */
const assembler = {
  // Pending header bytes (until we have all 13)
  headerBuf: new Uint8Array(HEADER_BYTES),
  headerHave: 0,
  // Once header is parsed, we know the payload size and start
  // accumulating into payloadBuf.
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
      // Snapshot dims BEFORE paintFrame — it calls frame.close() in a
      // finally block, after which displayWidth/displayHeight return 0.
      const w = frame.displayWidth
      const h = frame.displayHeight
      statsLastWidth = w
      statsLastHeight = h
      paintFrame(frame)
      // rc.187 — report dims on frame 1 AND on every size change (live
      // downscale), so the composable updates the cursor-mapping intrinsic.
      if (w !== lastPostedW || h !== lastPostedH) {
        lastPostedW = w
        lastPostedH = h
        workerScope.postMessage({ type: 'first-frame', width: w, height: h })
      }
      if (framesDecoded > 1) {
        // Composable-side counter consumes this for view diagnostics
        // and tests; we deliberately do NOT include the VideoFrame
        // itself in the message (already closed by paintFrame, and
        // it'd serialise as a copy here anyway).
        workerScope.postMessage({ type: 'frame-decoded', count: framesDecoded })
      }
      // Emit on every output too so dims update promptly during
      // resolution changes (rc:resolution / DPI flip / monitor swap).
      maybeEmitStats()
    },
    error: (err) => {
      workerScope.postMessage({
        type: 'decoder-error',
        error: extractErrorMessage(err),
      })
    },
  })
  // Configure now; the default `vp09.01.10.08` is unconditionally
  // supported by WebCodecs in Chromium-based browsers (libvpx ships
  // in-tree). If a future Chrome deprecates profile 1 we'll see the
  // rejection in the error callback. Tests may override via
  // `init-canvas.codec`.
  try {
    decoder.configure({
      codec: activeCodec,
      optimizeForLatency: true,
      // Hardware-decode preference. Profile-1 (4:4:4) HW decode is
      // rare — Intel Tiger Lake+, some AMD; NVDEC and Intel UHD 630
      // don't expose it. `'prefer-hardware'` was tried (added in
      // aac736b) on the theory Chromium would silently fall back to
      // SW decode on GPUs without 4:4:4 support — but on Chrome 148
      // it does NOT: `configure()` accepts the hint, then the first
      // real frame hard-rejects with "Unsupported configuration",
      // the decoder closes, and the canvas goes black (field repro
      // 2026-05-21, RTX 5090 + UHD 630). `'no-preference'` lets
      // Chromium pick HW decode where it genuinely exists and SW
      // otherwise — the actual silent-fallback behaviour we want.
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
      // Still collecting the 13-byte header.
      const need = HEADER_BYTES - assembler.headerHave
      const take = Math.min(need, bytes.length - cursor)
      assembler.headerBuf.set(
        bytes.subarray(cursor, cursor + take),
        assembler.headerHave,
      )
      assembler.headerHave += take
      cursor += take
      if (assembler.headerHave === HEADER_BYTES) {
        // Header complete — parse it.
        const view = new DataView(
          assembler.headerBuf.buffer,
          assembler.headerBuf.byteOffset,
          HEADER_BYTES,
        )
        const size = view.getUint32(0, true /* little-endian */)
        const flags = view.getUint8(4)
        const ts_lo = view.getUint32(5, true)
        const ts_hi = view.getUint32(9, true)
        const ts_us = (BigInt(ts_hi) << 32n) | BigInt(ts_lo)
        if (size === 0 || size > 16 * 1024 * 1024) {
          // Out-of-spec; drop and resync. 16 MB cap is generous —
          // a 4K I444 keyframe at very high bitrate is ~6 MB.
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
      // Filling payload.
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
        // Reset for the next frame.
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
  if (!shouldDecodeFrame(sawKeyframe, isKey)) {
    framesSkippedAwaitingKey++
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
  // rc.130 — backlog shed. Drop this delta + re-arm the keyframe gate (so we
  // keep dropping deltas, which would otherwise decode-error against the
  // missing reference) until the resync IDR lands.
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
  // EncodedVideoChunk.timestamp is a microsecond integer per spec.
  // We pass the agent-side capture timestamp through unmodified —
  // VideoDecoder uses it for ordering / frame.timestamp passthrough.
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

function paintFrame(frame: VideoFrame): void {
  if (!canvas || !ctx) {
    frame.close()
    return
  }
  try {
    if (canvas.width !== frame.displayWidth) canvas.width = frame.displayWidth
    if (canvas.height !== frame.displayHeight) canvas.height = frame.displayHeight
    ctx.drawImage(frame, 0, 0)
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
  lastPostedW = 0
  lastPostedH = 0
  // rc.103 — re-arm the leading-delta gate for the next decoder.
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
 *  frame header. Lets the wire format stay regression-tested
 *  without standing up a full WebCodecs harness. */
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

/** rc.103 — leading-delta gate decision, pure for vitest. A delta is only
 *  decodable once a keyframe has been seen; a keyframe is always decodable.
 *  Mirrors rc-hevc-worker so the wire-level invariant is shared. */
export function shouldDecodeFrame(hasSeenKeyframe: boolean, isKey: boolean): boolean {
  return hasSeenKeyframe || isKey
}
