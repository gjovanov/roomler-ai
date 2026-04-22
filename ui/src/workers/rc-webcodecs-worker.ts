/// <reference lib="webworker" />

/**
 * Tier B7 — WebCodecs render path.
 *
 * Web Worker that receives encoded video frames from an
 * `RTCRtpScriptTransform` on the main thread, decodes them via
 * `VideoDecoder`, and paints the results onto an `OffscreenCanvas`.
 * Bypasses Chrome's built-in jitter buffer on the `<video>` element
 * (the ~80 ms soft floor that keeps the browser viewer perceptually
 * laggier than RustDesk's native path).
 *
 * The worker is Chrome-only in practice: Firefox exposes insertable
 * streams via a different API and Safari lands it piecemeal. The
 * composable feature-detects `RTCRtpScriptTransform` + `VideoDecoder`
 * and falls back to classic `<video>` rendering when either is
 * missing.
 *
 * Flow: main thread posts `init-canvas` with a transferred
 * OffscreenCanvas; then assigns `receiver.transform = new
 * RTCRtpScriptTransform(worker, { codec })`. That fires
 * `self.onrtctransform` here, which opens a VideoDecoder, pipes the
 * transformer's readable into it, and paints decoded frames to the
 * canvas. We intentionally do NOT write frames to the transformer's
 * writable — that swallows the stream's default path so the `<video>`
 * element would render black (fine; we're rendering the canvas
 * instead).
 */

type InitCanvasMessage = { type: 'init-canvas'; canvas: OffscreenCanvas }
type CloseMessage = { type: 'close' }
type IncomingMessage = InitCanvasMessage | CloseMessage

interface RTCTransformerLike {
  readable: ReadableStream<unknown>
  writable: WritableStream<unknown>
  options?: { codec?: string }
}

interface RTCTransformEventLike {
  transformer: RTCTransformerLike
}

interface EncodedFrameLike {
  type?: string
  timestamp: number
  data: ArrayBuffer
}

const workerScope = self as unknown as {
  onmessage: ((ev: MessageEvent<IncomingMessage>) => void) | null
  onrtctransform: ((ev: RTCTransformEventLike) => void) | null
  postMessage: (msg: unknown) => void
}

let canvas: OffscreenCanvas | null = null
let ctx: OffscreenCanvasRenderingContext2D | null = null
let decoder: VideoDecoder | null = null
let configured = false
let framesDecoded = 0

workerScope.onmessage = (e) => {
  const msg = e.data
  if (!msg) return
  if (msg.type === 'init-canvas') {
    canvas = msg.canvas
    ctx = canvas.getContext('2d')
  } else if (msg.type === 'close') {
    try {
      decoder?.close()
    } catch {
      /* idempotent teardown */
    }
    decoder = null
    canvas = null
    ctx = null
    configured = false
    framesDecoded = 0
  }
}

workerScope.onrtctransform = async (event) => {
  const transformer = event.transformer
  const codec = transformer.options?.codec ?? 'h264'
  const codecString = codecMimeForShort(codec)

  decoder = new VideoDecoder({
    output: (frame) => {
      framesDecoded++
      paintFrame(frame)
      if (framesDecoded === 1) {
        workerScope.postMessage({
          type: 'first-frame',
          width: frame.displayWidth,
          height: frame.displayHeight,
        })
      }
    },
    error: (err) => {
      workerScope.postMessage({
        type: 'decoder-error',
        error: extractErrorMessage(err),
      })
    },
  })

  // Try the "latency-first" config first, then fall back to a plain
  // config. Some Chrome builds reject `optimizeForLatency: true` for
  // HEVC specifically (the flag landed after the HEVC decoder did);
  // the plain config usually works when the codec itself is
  // supported. If NEITHER works, the worker still stands up but
  // every decode() call will error — surface `decoder-error` from
  // main-thread console.
  const configs: VideoDecoderConfig[] = [
    { codec: codecString, optimizeForLatency: true } as VideoDecoderConfig,
    { codec: codecString } as VideoDecoderConfig,
  ]
  let picked: VideoDecoderConfig | null = null
  for (const cfg of configs) {
    try {
      const support = await VideoDecoder.isConfigSupported(cfg)
      if (support.supported) {
        picked = cfg
        break
      }
    } catch {
      // isConfigSupported can reject on older Chromes; try configure() below anyway.
    }
  }
  const toUse = picked ?? configs[0]!
  try {
    decoder.configure(toUse)
    configured = true
  } catch (err) {
    workerScope.postMessage({
      type: 'decoder-configure-error',
      codec: codecString,
      error: extractErrorMessage(err),
    })
  }

  workerScope.postMessage({
    type: 'transform-active',
    codec,
    codecString,
    configured,
    pickedConfig: picked ? 'with-optimize-for-latency' : 'fallback',
  })

  const reader = transformer.readable.getReader()
  ;(async () => {
    try {
      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        if (!value || !decoder) continue
        const encFrame = value as unknown as EncodedFrameLike
        if (!encFrame.data || !(encFrame.data instanceof ArrayBuffer)) continue
        if (!configured) {
          // VideoDecoder.configure() can fail synchronously on an
          // unknown codec string, but a later incoming frame might
          // still match a compatible profile. Retry once.
          try {
            decoder.configure({
              codec: codecString,
              optimizeForLatency: true,
            } as VideoDecoderConfig)
            configured = true
          } catch {
            continue
          }
        }
        try {
          const chunk = new EncodedVideoChunk({
            type: encFrame.type === 'key' ? 'key' : 'delta',
            timestamp: encFrame.timestamp,
            data: encFrame.data,
          })
          decoder.decode(chunk)
        } catch (err) {
          workerScope.postMessage({
            type: 'decode-error',
            error: extractErrorMessage(err),
          })
        }
      }
    } catch (err) {
      workerScope.postMessage({
        type: 'reader-error',
        error: extractErrorMessage(err),
      })
    }
  })()
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
    /* canvas vanished mid-teardown */
  } finally {
    frame.close()
  }
}

function extractErrorMessage(err: unknown): string {
  if (err instanceof Error) return err.message
  try {
    return String(err)
  } catch {
    return 'unknown'
  }
}

/**
 * Map a short codec name (from our protocol) to the codec string
 * `VideoDecoder.configure()` expects. These are permissive defaults;
 * Chrome tolerates variants when decoding Annex-B RTP NALUs from
 * webrtc-rs's depayloader. Exported for vitest.
 */
export function codecMimeForShort(short: string): string {
  switch (short.toLowerCase()) {
    case 'h264':
      return 'avc1.42E01F'
    case 'h265':
    case 'hevc':
      return 'hev1.1.6.L93.B0'
    case 'av1':
      return 'av01.0.08M.08'
    case 'vp9':
      return 'vp09.00.10.08'
    case 'vp8':
      return 'vp8'
    default:
      return 'avc1.42E01F'
  }
}
