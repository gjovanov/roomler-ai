// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
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

import { epochNowMs } from './rc-hop-stats'
import { createBenchSampler } from './rc-bench-sampler'
import { createDecoderColorSpace, frameColorSpace, rcDecoderConfig } from './rc-decoder-config'

/** `bench` — FR-16 L4 bench mode, set only when the knob is on AND the
 *  agent advertised `bench`: sample the probe marker while a probe is armed. */
type InitCanvasMessage = { type: 'init-canvas'; canvas: OffscreenCanvas; bench?: boolean }
/** FR-16 L4 — the main thread just sent bench probe `seq`. */
type BenchArmMessage = { type: 'bench-arm'; seq: number }
type CloseMessage = { type: 'close' }
type IncomingMessage = InitCanvasMessage | BenchArmMessage | CloseMessage

interface RTCTransformerLike {
  readable: ReadableStream<unknown>
  writable: WritableStream<unknown>
  /** The composable's `new RTCRtpScriptTransform(worker, options)`.
   *  `colorSpace` — FR-130 P2: tell the decoder the colour space the agent
   *  converted with (set S; `rc-decoder-config.ts`), resolved per session by
   *  the composable (`sessionColorSpace`). It rides the options rather than
   *  `init-canvas` because this transform can start, and configure the
   *  decoder, before the canvas arrives. Absent/false configures as before. */
  options?: { codec?: string; colorSpace?: boolean }
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

/** FR-16 L4 — reads the bench probe off painted frames, only while armed. */
const bench = createBenchSampler({ post: (m) => workerScope.postMessage(m), now: epochNowMs })

let canvas: OffscreenCanvas | null = null
let ctx: OffscreenCanvasRenderingContext2D | null = null
let decoder: VideoDecoder | null = null
let configured = false
let framesDecoded = 0
let framesReceived = 0
let framesFedToDecoder = 0
/** The codec string this transform decodes, for the `colorspace-dropped` line. */
let activeCodecString = ''

/** FR-130 P2 — the session's decoder colour space, and its one fallback: a
 *  browser that refuses it gets the config without it (one console line on
 *  the main thread, from `colorspace-dropped`). */
const decoderColorSpace = createDecoderColorSpace((stage, error) =>
  workerScope.postMessage({
    type: 'colorspace-dropped',
    stage,
    codec: activeCodecString,
    error: extractErrorMessage(error),
  }),
)
/** FR-130 P2 — set when the decoder was rebuilt without the colour space. A
 *  new decoder takes a key frame first, so until one arrives the deltas go on
 *  to the default path undecoded, and the agent is asked for one. */
let awaitingKeyAfterRebuild = false
let lastKeyframeReqMs = 0

/** Ask the agent (the composable → `rc:keyframe` on the control channel) for
 *  a key frame; debounced like the data-channel workers' resync request.
 *  `now` skips the debounce, for the once-a-session rebuild: early in a
 *  worker's life the debounce would swallow it (the clock starts at the
 *  worker's creation). */
function requestKeyframe(opts: { now?: boolean } = {}): void {
  const now = performance.now()
  if (!opts.now && now - lastKeyframeReqMs < 250) return
  lastKeyframeReqMs = now
  workerScope.postMessage({ type: 'request-keyframe' })
}

workerScope.onmessage = (e) => {
  const msg = e.data
  if (!msg) return
  if (msg.type === 'init-canvas') {
    canvas = msg.canvas
    ctx = canvas.getContext('2d')
    if (typeof msg.bench === 'boolean') bench.setEnabled(msg.bench)
  } else if (msg.type === 'bench-arm') {
    bench.arm(msg.seq)
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
    awaitingKeyAfterRebuild = false
  }
}

workerScope.onrtctransform = async (event) => {
  const transformer = event.transformer
  const codec = transformer.options?.codec ?? 'h264'
  const codecString = codecMimeForShort(codec)
  activeCodecString = codecString
  // FR-130 P2 — set before the decoder is configured below.
  decoderColorSpace.setWanted(transformer.options?.colorSpace === true)
  /** The config the decoder was last configured from, without its colour
   *  space: what a rebuild configures again. */
  let configuredBase: VideoDecoderConfig | null = null

  function makeDecoder(): VideoDecoder {
    return new VideoDecoder({
      output: (frame) => {
        framesDecoded++
        // Snapshot before paint closes the frame (a closed one reports 0).
        const firstDims = framesDecoded === 1 ? { w: frame.displayWidth, h: frame.displayHeight } : null
        // FR-130 P2 — and the colour space the decoder says it produced.
        const colorSpace = firstDims ? frameColorSpace(frame) : null
        // FR-16 L4 — a clone for the probe read; null unless a probe is armed.
        const benchClone = bench.take(frame)
        paintFrame(frame)
        if (benchClone) bench.settle(benchClone, epochNowMs())
        if (firstDims) {
          workerScope.postMessage({
            type: 'first-frame',
            width: firstDims.w,
            height: firstDims.h,
            colorSpace,
          })
        }
      },
      error: (err) => {
        // FR-130 P2 — a decoder told the colour space that fails before its
        // first frame gets one more try without it, so a browser that
        // refuses the member or a value in it keeps this path. Any other
        // error is reported as before (the composable falls back to <video>).
        if (decoderColorSpace.retryAfterError(err, framesDecoded)) {
          rebuildWithoutColorSpace()
          return
        }
        workerScope.postMessage({
          type: 'decoder-error',
          error: extractErrorMessage(err),
        })
      },
    })
  }

  /** FR-130 P2 — an error closes a VideoDecoder: start a new one, configured
   *  without the colour space (`decoderColorSpace` has dropped it), and wait
   *  for a key frame. */
  function rebuildWithoutColorSpace(): void {
    try {
      decoder?.close()
    } catch {
      /* already closed by its error */
    }
    decoder = makeDecoder()
    configured = false
    const base = configuredBase ?? ({ codec: codecString, optimizeForLatency: true } as VideoDecoderConfig)
    try {
      decoderColorSpace.configure(decoder, (colorSpace) => rcDecoderConfig(base, colorSpace))
      configured = true
    } catch (err) {
      workerScope.postMessage({
        type: 'decoder-configure-error',
        codec: codecString,
        error: extractErrorMessage(err),
      })
    }
    awaitingKeyAfterRebuild = true
    requestKeyframe({ now: true })
  }

  decoder = makeDecoder()

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
  // FR-130 P2 — the probes above stay codec-only; the colour space joins only
  // the config configure() takes, and a configure() that throws on it goes
  // again without it (`rc-decoder-config.ts`).
  let applied: VideoDecoderConfig | null = null
  try {
    applied = decoderColorSpace.configure(decoder, (colorSpace) => rcDecoderConfig(toUse, colorSpace))
    configuredBase = toUse
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
    // FR-130 P2 — the colour space configure() took, or null.
    colorSpace: applied?.colorSpace ?? null,
  })

  // Watchdog: if 3 seconds pass after onrtctransform fires and no
  // frame callback has run, surface a loud warning. Lets us
  // distinguish "Chrome doesn't forward HEVC to scripted transforms
  // at all" from "it does but our decoder silently drops". Armed
  // once per onrtctransform call; cleared by the first read.
  let watchdogFired = false
  const watchdog = setTimeout(() => {
    if (framesReceived === 0 && !watchdogFired) {
      watchdogFired = true
      workerScope.postMessage({
        type: 'watchdog',
        message: 'no encoded frames in 3s after transform-active — Chrome may be holding frames on the default path',
      })
    }
  }, 3000)

  // Canonical RTCRtpScriptTransform pattern. We pipe frames through
  // a TransformStream to `transformer.writable` (the default decoder
  // path) because Chrome's receive-side transform uses the writable
  // as the demand source — an unconsumed writable eventually
  // back-pressures the readable. The transform callback decodes each
  // frame in parallel via our VideoDecoder → canvas, and enqueues
  // the original frame unchanged so the default pipeline stays fed.
  const inFlight = transformer.readable
    .pipeThrough(
      new TransformStream<unknown, unknown>({
        transform: (value, controller) => {
          // Log EVERY callback entry on the first 3 hits to prove
          // the transform actually runs. Many black-screen failure
          // modes show up as "transform never called once".
          framesReceived++
          if (framesReceived <= 3) {
            clearTimeout(watchdog)
            const sig = (() => {
              try {
                const f = value as unknown as EncodedFrameLike
                if (f?.data && f.data instanceof ArrayBuffer) {
                  const bytes = new Uint8Array(f.data)
                  return {
                    byteLength: f.data.byteLength,
                    frameType: f.type ?? 'unknown',
                    firstBytes: Array.from(bytes.slice(0, 8))
                      .map((b) => b.toString(16).padStart(2, '0'))
                      .join(' '),
                  }
                }
                return { byteLength: 0, frameType: typeof value }
              } catch {
                return { byteLength: -1, frameType: 'inspect-threw' }
              }
            })()
            workerScope.postMessage({
              type: framesReceived === 1 ? 'first-encoded-frame' : 'early-encoded-frame',
              idx: framesReceived,
              ...sig,
            })
          }
          if (framesReceived % 30 === 0) {
            workerScope.postMessage({
              type: 'reader-heartbeat',
              received: framesReceived,
              fedToDecoder: framesFedToDecoder,
              decoded: framesDecoded,
            })
          }
          // Defensive: if the value isn't what we expect, still
          // forward it so the pipeline doesn't stall on our filter.
          const encFrame = value as unknown as EncodedFrameLike
          if (
            !decoder
            || !encFrame
            || !encFrame.data
            || !(encFrame.data instanceof ArrayBuffer)
          ) {
            controller.enqueue(value)
            return
          }
          if (!configured) {
            const lazyBase = {
              codec: codecString,
              optimizeForLatency: true,
            } as VideoDecoderConfig
            try {
              // FR-130 P2 — through the colour-space helper, like the first try.
              decoderColorSpace.configure(decoder, (colorSpace) => rcDecoderConfig(lazyBase, colorSpace))
              configuredBase = lazyBase
              configured = true
            } catch {
              controller.enqueue(value)
              return
            }
          }
          // FR-130 P2 — a rebuilt decoder takes a key frame first; until one
          // arrives, the deltas still go on to the default path below.
          if (awaitingKeyAfterRebuild) {
            if (encFrame.type !== 'key') {
              requestKeyframe()
              controller.enqueue(value)
              return
            }
            awaitingKeyAfterRebuild = false
          }
          try {
            const chunk = new EncodedVideoChunk({
              type: encFrame.type === 'key' ? 'key' : 'delta',
              timestamp: encFrame.timestamp,
              data: encFrame.data,
            })
            decoder.decode(chunk)
            framesFedToDecoder++
          } catch (err) {
            workerScope.postMessage({
              type: 'decode-error',
              error: extractErrorMessage(err),
            })
          }
          controller.enqueue(value)
        },
      }),
    )
    .pipeTo(transformer.writable)
  inFlight.catch((err) => {
    workerScope.postMessage({
      type: 'pipe-error',
      error: extractErrorMessage(err),
    })
  })
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
      // rc.94 — Level 5.1 (`L153`), not L3.1 (`L93`). L3.1 maxes at
      // ~1280×720; desktops at 1920×1200 / 1440p / 4K exceed it and the
      // HW HEVC decoder renders black. L5.1 (up to 4K@64) is a declared
      // MAX, so smaller streams still decode. Matches rc-hevc-worker's
      // DEFAULT_HEVC_CODEC + the isHevcDecodeSupported probe.
      return 'hev1.1.6.L153.B0'
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
