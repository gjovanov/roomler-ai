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
    consumeBytes(new Uint8Array(msg.bytes))
  } else if (msg.type === 'close') {
    teardown()
  }
}

function initDecoder() {
  if (decoder) return
  decoder = new VideoDecoder({
    output: (frame) => {
      framesDecoded++
      // Snapshot dims BEFORE paintFrame — it calls frame.close() in a
      // finally block, after which displayWidth/displayHeight return 0.
      const w = frame.displayWidth
      const h = frame.displayHeight
      paintFrame(frame)
      if (framesDecoded === 1) {
        workerScope.postMessage({ type: 'first-frame', width: w, height: h })
      } else {
        // Composable-side counter consumes this for view diagnostics
        // and tests; we deliberately do NOT include the VideoFrame
        // itself in the message (already closed by paintFrame, and
        // it'd serialise as a copy here anyway).
        workerScope.postMessage({ type: 'frame-decoded', count: framesDecoded })
      }
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
