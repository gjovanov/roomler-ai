// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/// <reference lib="webworker" />

/**
 * FR-135 R3 — the one decoder worker for video over the data channel. Every
 * codec the agent sends on `video-bytes` comes here: VP9 profile 0 and 1, AV1,
 * H.264 and HEVC (Main and Rext). The composable names the codec in
 * `init-canvas`; the wire format, the decoder's life, the paint and the
 * numbers are the same for all of them. It was two workers until R3:
 * `rc-vp9-444-worker.ts` (Phase Y.2, carrying AV1 since rc.190 and H.264
 * since P2) and its copy for HEVC, `rc-hevc-worker.ts` (rc.78). See
 * `docs/remote-control.md` §11.14 and `docs/encoders.md`.
 *
 * Receives length-prefixed encoded frames, reassembles them, feeds
 * `VideoDecoder`, and paints the resulting `VideoFrame`s to an
 * `OffscreenCanvas`. The frames come one of two ways: the main thread forwards
 * each `RTCDataChannel` message as a `chunk` (the relay, and the default), or,
 * where the browser can transfer the channel and `roomler-rc-dc-worker` says
 * so, the page moves the channel itself here (`video-channel`, FR-135 R3) and
 * the main thread is out of the path. No `RTCRtpScriptTransform` involvement —
 * this worker is independent of the broken-in-Chrome 131 RTP transform path.
 *
 * Wire format (`frame_video_bytes`, `agents/roomlerd/src/peer.rs`):
 *
 *   struct Frame {
 *     u32  size_le;          // payload length, little-endian
 *     u8   flags;            // bit 0: keyframe
 *     u64  timestamp_us;     // the agent's process clock at framing
 *                            // (`agent_epoch_us()`), not the capture time
 *     [u8] payload;          // the codec's frame: VP9 or AV1 as encoded,
 *                            // H.264 and HEVC as Annex-B access units
 *                            // (HEVC key frames AUD → VPS → SPS → PPS → IDR)
 *   }
 *
 * Header is 13 bytes. SCTP is reliable+ordered so a single frame
 * may arrive across multiple `onmessage` chunks; the assembler in
 * this worker concatenates by tracking how many bytes of the
 * declared payload size remain.
 */

import {
  HopStats,
  QueueDrift,
  ctxOptionsFor,
  epochNowMs,
  epochNowUs,
  frameAgeMs,
  normalizeCtxMode,
  normalizeIntKnob,
  round1,
  DEFAULT_MAX_BACKLOG_MS,
  DecodeBacklog,
  backlogShouldDrop,
  type CtxMode,
} from './rc-hop-stats'
import {
  FsrRenderer,
  computeRenderTarget,
  normalizeSharpenMode,
  normalizeSharpness,
  DEFAULT_RCAS_SHARPNESS,
  type RenderPass,
  type SharpenMode,
} from './rc-fsr-render'
import {
  createChunkFraming,
  stripChunkPrefix,
} from './rc-chunk-framing'
import { createBenchSampler } from './rc-bench-sampler'
import { startViewerHzBursts } from './rc-viewer-hz'
import { createDecoderColorSpace, frameColorSpace, rcDecoderConfig } from './rc-decoder-config'
import { createDecodeRecovery } from './rc-decode-recovery'
import { createViewerMediaRecorder } from './rc-media-window'
import { createDeliveryGapClock, type VideoChannelMessage } from './rc-video-channel'
import {
  clockCalPong,
  fwdHopMs,
  normalizeSkewMs,
  toPageEpochMs,
  workerAgentOffsetUs,
  type ClockCalMessage,
  type ClockCalPing,
} from './rc-clock-cal'

type InitCanvasMessage = {
  type: 'init-canvas'
  canvas: OffscreenCanvas
  /** FR-16 L4 — bench mode: sample the probe marker while a probe is armed.
   *  Set only when the knob is on AND the agent advertised `bench`; sticks
   *  across the idempotent visible-canvas re-init, like `ctxMode`. */
  bench?: boolean
  /** FR-130 P2 — tell the decoder the colour space the agent converted with
   *  (set S, BT.601 limited; `rc-decoder-config.ts`). Resolved per session by
   *  the composable (`sessionColorSpace`, off under the kill switch
   *  `roomler-rc-colorspace=off`). Absent/false configures as before FR-130,
   *  and it sticks across the idempotent visible-canvas re-init. */
  colorSpace?: boolean
  /** FR-135 R1 — replace a decoder that fails after its first frame, in place
   *  (`rc-decode-recovery.ts`). Resolved per session by the composable
   *  (`sessionDecodeRecover`, off under `roomler-rc-decode-recover=0`).
   *  Absent/false reports the failure as before, and it sticks across the
   *  idempotent visible-canvas re-init. */
  decodeRecover?: boolean
  /** FR-135 R1 — this session's video channel is unordered (`videoDcOptions`),
   *  so a chunk gap is routine and keeps the deltas moving. Sticks the same way. */
  unorderedVideo?: boolean
  /** FR-17 — true when the controller negotiated per-chunk framing for
   *  this session (the agent advertised `chunk-framing` in
   *  `AgentCaps.video`). Absent/false keeps the pre-FR-17 bare byte
   *  stream, which is what every agent below 0.4.14 still sends. */
  chunkFraming?: boolean
  /** The `VideoDecoder` codec string for this session's cell. The composable
   *  names one for every session: `vp09.01.10.08` / `vp09.00.10.08` (VP9
   *  4:4:4 / 4:2:0), `AV1_CODEC_STRING`, the H.264 string its probe ladder
   *  picked, `HEVC_MAIN_CODEC_STRING` or `HEVC_REXT_CODEC_STRING`. Absent, it
   *  is VP9 profile 1 ({@link DEFAULT_CODEC}): the e2e harness, which has no
   *  4:4:4 *encoder* in current Chromium WebCodecs, names VP9 profile 0 to
   *  exercise the wire, the channel and the decoder. */
  codec?: string
  /** 2026-07-24 decode-stall A/B — optional `VideoDecoder.hardwareAcceleration`
   *  override from the composable (localStorage `roomler-rc-decode-pref`).
   *  Unset → `'no-preference'`, today's behaviour. */
  decodePref?: 'prefer-software' | 'prefer-hardware' | 'no-preference'
  /** P1 — 2D-context A/B mode (localStorage `roomler-rc-ctx-mode`).
   *  Remembered across the idempotent visible-canvas re-init (which omits
   *  it), so the swapped-in canvas keeps the same context options. */
  ctxMode?: string
  /** P1 — restore the legacy per-decoded-frame `frame-decoded` postMessage
   *  (localStorage `roomler-rc-per-frame-msg`). Default OFF: it was ~60
   *  structured-clone messages + 60 Vue reactive increments per second of
   *  pure main-thread overhead; the 1 s `stats` message carries
   *  `framesDecodedTotal` instead. */
  perFrameMsg?: boolean
  /** FR-119 P2b-7 — the backlog-drop threshold in ms (localStorage
   *  `roomler-rc-max-backlog-ms`; was P6's frame-count `maxQueue`). Sticks
   *  across the idempotent visible-canvas re-init (which omits it), like
   *  `ctxMode`. */
  maxBacklogMs?: number
  /** P7 — FSR sharpening mode (localStorage `roomler-rc-sharpen`).
   *  Sticks across the idempotent visible-canvas re-init, like `ctxMode`. */
  sharpen?: string
  /** P7 — RCAS sharpness stops (localStorage `roomler-rc-fsr-sharpness`). */
  sharpness?: number
}
type ChunkMessage = {
  type: 'chunk'
  bytes: ArrayBuffer
  /** P1 — main-thread epoch timestamp (timeOrigin + now) stamped at the DC
   *  `onmessage` → measures the main→worker forwarding hop. On the page's
   *  clock, so it is read through `pageSkewMs` (#3566). */
  sentAt?: number
}
/** P7 — the canvas element's CSS box + devicePixelRatio (composable
 *  ResizeObserver). Drives the FSR sizing policy; until the first report
 *  the worker stays on the 1:1 blit path. */
type ViewportMessage = { type: 'viewport'; cssW: number; cssH: number; dpr: number }
/** P7 — live sharpen-mode/sharpness change from the settings UI. */
type RenderModeMessage = { type: 'render-mode'; sharpen?: string; sharpness?: number }
/** FR-1 P7 — agent-clock offset from the main thread's rc:clock probe: the
 *  agent's clock against the page's, read here through `pageSkewMs` (#3566). */
type ClockOffsetMessage = { type: 'clock-offset'; offsetUs: number | null }
/** FR-16 L4 — the main thread just sent bench probe `seq`. */
type BenchArmMessage = { type: 'bench-arm'; seq: number }
/** FR-135 R0c — the agent announced a capture hold (`rc:media-waiting`). */
type MediaHoldMessage = { type: 'media-hold' }
type CloseMessage = { type: 'close' }
type IncomingMessage =
  | InitCanvasMessage
  | ChunkMessage
  | VideoChannelMessage
  | ViewportMessage
  | RenderModeMessage
  | ClockOffsetMessage
  | ClockCalPing
  | ClockCalMessage
  | BenchArmMessage
  | MediaHoldMessage
  | CloseMessage

const HEADER_BYTES = 13
const FLAG_KEYFRAME = 0x01
/** The codec when `init-canvas` names none: VP9 profile 1 (4:4:4 8-bit), the
 *  one this worker decoded when it was the VP9 worker. Browsers' WebCodecs
 *  decoders use libvpx and accept it without quibble. */
const DEFAULT_CODEC = 'vp09.01.10.08'
let activeCodec: string = DEFAULT_CODEC

const workerScope = self as unknown as {
  onmessage: ((ev: MessageEvent<IncomingMessage>) => void) | null
  onmessageerror: ((ev: MessageEvent) => void) | null
  postMessage: (msg: unknown) => void
}

/** FR-16 L4 — reads the bench probe off painted frames, only while armed. */
const bench = createBenchSampler({ post: (m) => workerScope.postMessage(m), now: epochNowMs })

/** FR-130 P2 — the session's decoder colour space, and its one fallback: a
 *  browser that refuses it gets the config without it (one console line on
 *  the main thread, from `colorspace-dropped`). */
const decoderColorSpace = createDecoderColorSpace((stage, error) =>
  workerScope.postMessage({
    type: 'colorspace-dropped',
    stage,
    codec: activeCodec,
    error: extractErrorMessage(error),
  }),
)

/** FR-135 R1 — whether a failed decoder is replaced in place. */
const decodeRecovery = createDecodeRecovery()

/** FR-135 R0 — the window of media numbers the page carries to the agent on
 *  `rc:decodestat` (`rc-media-window.ts`). Records only. */
const mediaWindow = createViewerMediaRecorder()

let canvas: OffscreenCanvas | null = null
let ctx: OffscreenCanvasRenderingContext2D | null = null
let decoder: VideoDecoder | null = null
let configured = false
let framesDecoded = 0
let framesReceived = 0
// rc.103 — leading-delta gate. A VideoDecoder rejects a delta as its FIRST
// input after configure()/flush() ("A key frame is required"). The agent's
// FFmpeg encoders (NVENC, QSV, AMF: HEVC, AV1, H.264, vp9_qsv) are ASYNC
// (pipeline depth): at the instant the DC reaches Open, the packet draining
// is a buffered delta from earlier in the encoder queue — it ships ahead of
// the freshly-forced IDR (rc.97 force-kf-on-open), so the browser's first
// ASSEMBLED frame is sometimes a delta. Field WINHOST-G (hevc_qsv) hit
// exactly this: "hevc DC opened" → "decode-error: A key frame is required" →
// permanent <video> fallback. The libvpx VP9 pump is synchronous (the first
// emitted packet is already the IDR), so the gate normally passes at once
// there. Drop leading deltas until the first keyframe; the IDR is only a
// handful of frames behind them in the async queue, so the wait is a few ms.
let sawKeyframe = false
let framesSkippedAwaitingKey = 0

// rc.130 — decode-backlog shed. With `optimizeForLatency` the decoder keeps
// its input queue at 0–1 when it's keeping up; a sustained queue above this
// means it has fallen behind (slow client / 4K on a weak iGPU / throttled
// background tab) and the stream latency is growing monotonically. When that
// happens we drop incoming DELTAS and ask the agent for a fresh keyframe to
// resync — never dropping a key frame (it's the resync point). Without this
// the queue never drains and "typing appears seconds later" is permanent.
// rc.188 — raised 2→4: the agent now caps send-fps to this viewer's reported
// sustainable rate (`rc:decodestat`), so in steady state the queue sits at ~0.
// The higher ceiling only matters transiently — it lets an onset burst (the
// first ~1 s of a window-drag, before the agent's cap engages) ride through
// WITHOUT the destructive drop+IDR-storm that used to freeze the screen. The
// composable's `struggling` rule trips at a lower queue depth, so the cap
// reacts before we ever reach this drop threshold.
// P6 — init-canvas-configurable for the flow-control field-tuning round.
// FR-119 P2b-7 — the threshold is the decode BACKLOG in ms, not a frame count:
// 4 frames were 67 ms at 60 and only 33 ms at 120, where one slow keyframe
// tripped the drop and the drop's IDR tripped it again (`DecodeBacklog`).
let maxBacklogMs = DEFAULT_MAX_BACKLOG_MS
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

// 2026-07-24 decode-stall telemetry — field: DEVBOX viewing WINHOST-A hits 3-5 s
// mid-session stalls where bytes keep ARRIVING (Mbps > 0) but the decoder
// produces NO output, then a catch-up burst (queued frames flushing) — on
// both HEVC and VP9, with the agent proven healthy. That shape points at the
// decoder/GPU-process layer, not transport and not the resync gate. Track the
// output-callback cadence: if chunks keep landing while output has been
// silent > STALL_GAP_MS with work queued, report a one-shot `decode-stall`
// (and `decode-stall-recovered` with the true gap once output resumes) so the
// console pins the layer during field repros. `decodePref` is the
// composable's localStorage A/B override for `hardwareAcceleration`. HEVC has
// no software decoder in Chromium: prefer-software there fails configure()
// and the composable's fallback takes over, so the A/B means something on
// VP9, AV1 and H.264.
const STALL_GAP_MS = 1500
let decodePref: 'prefer-software' | 'prefer-hardware' | 'no-preference' = 'no-preference'
let lastOutputMs = 0
let stallSince = 0 // 0 = no active stall; else the lastOutputMs the stall started from

function noteChunkForStallCheck(): void {
  if (!configured || framesDecoded === 0 || stallSince !== 0 || lastOutputMs === 0) return
  const now = performance.now()
  // 2026-07-24 round 3 — fire even with an EMPTY queue: during a gate wedge
  // the deltas are dropped before decode() so the queue sits at 0, and the
  // original queue>0 requirement made the detector silent for exactly the
  // freeze it was built to catch (field: round-1 freezes with zero console
  // output). `gated` says whether the keyframe gate was armed at the time.
  if (now - lastOutputMs > STALL_GAP_MS) {
    stallSince = lastOutputMs
    workerScope.postMessage({
      type: 'decode-stall',
      gapMs: Math.round(now - lastOutputMs),
      queue: decoder?.decodeQueueSize ?? 0,
      gated: !sawKeyframe,
    })
  }
}

function noteOutputForStallCheck(): void {
  const now = performance.now()
  // P1 — max inter-output gap per stats window (sub-stall stutter that the
  // 1500 ms stall detector never sees).
  if (lastOutputMs > 0) {
    const gap = now - lastOutputMs
    if (gap > outGapMaxMs) outGapMaxMs = gap
  }
  if (stallSince !== 0) {
    workerScope.postMessage({
      type: 'decode-stall-recovered',
      gapMs: Math.round(now - stallSince),
    })
    stallSince = 0
  }
  lastOutputMs = now
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

// P1 — per-hop instrumentation (see rc-hop-stats.ts). Folded into the 1 s
// stats message; zero extra message cadence.
let ctxMode: CtxMode = 'opaque-desync'
let perFrameMsg = false
const paintStats = new HopStats()
const fwdStats = new HopStats()
const decodeStats = new HopStats()
// FR-1 P7 — end-to-end age. The offset arrives from the main thread's
// rc:clock probe; until the first echo it stays null and the stats
// message reports `age: null` (HUD shows nothing rather than garbage).
const ageStats = new HopStats()
// FR-59 P3 — how much the transit queue grew this window. Needs NO clock
// probe (it is a difference of intervals), so unlike `ageStats` it keeps
// reporting on exactly the links where the probe is biased or absent.
// ⚠️ It is fed at PAINT, after the decode queue and the decoder, so a decoder
// falling behind reads as the link growing. The live law and FR-63's shadow
// read it as `queue_ms`, so it stays as it is (FR-135 §4.2).
const queueDrift = new QueueDrift()
// FR-135 R3 — the same growth read at ARRIVAL: every whole frame when its last
// chunk is in hand, before the key-frame gate, the backlog drop and the
// decoder, decoded or not. It goes beside the old reading as `queue_arr_ms`,
// and FR-63's B2 chooses between them.
const arrivalQueueDrift = new QueueDrift()
let clockOffsetUs: number | null = null
// #3566 — this worker's clock less the page's, ms, as the page measured it when
// it started the worker (`clock-cal`, `rc-clock-cal.ts`). Each context fixes its
// epoch from the wall clock when it starts, so a worker made after its page
// reads a constant apart from it. Every time the page stamped (`sentAt`, the
// agent offset above, a bench probe's send) is read through it. 0 until it
// arrives, and for a session that measures none: the reading of before, bit for
// bit. The worker's own differences (`queueMs`, `viewer`, `decode`) need none.
let pageSkewMs = 0
let outGapMaxMs = 0
// Decode-submission times keyed by chunk timestamp, matched in the output
// callback → submit→output latency, and (FR-119 P2b-7) the decode backlog the
// shed rule and the struggling rule read.
const backlog = new DecodeBacklog()
// FR-70 M0 — the age SPLIT. `ageStats` fuses sender, transit and viewer
// latency into one number, which is how a 4903 ms relay stall on 2026-09-04
// arrived looking like a pump problem. `arrivalStats` is the age of the
// frame when its LAST CHUNK reached this worker (agent clock via the probe,
// like `ageStats`), and `viewerStats` is the purely local time from that
// arrival to the paint (decode queue + decode + paint) — no probe needed.
// The agent subtracts: viewer = age − arrival, transit = arrival − its own
// send-queue wait. Keyed by the wire timestamp, like the decode `backlog`.
const arrivalStats = new HopStats()
const viewerStats = new HopStats()
const pendingArrivalUs = new Map<number, number>()

// P7 — FSR sharpening state (see rc-fsr-render.ts). The renderer is created
// lazily on the first frame that needs it; a GL failure disposes it and the
// next frame re-creates (bounded by MAX_FSR_REBUILDS, then the session goes
// permanently 2D with a one-shot `render-fallback` breadcrumb). The visible
// canvas stays 2D throughout, so every failure lands on the byte-identical
// legacy path the same frame.
const MAX_FSR_REBUILDS = 3
let sharpenMode: SharpenMode = 'auto'
let sharpness = DEFAULT_RCAS_SHARPNESS
let viewport: { cssW: number; cssH: number; dpr: number } | null = null
let fsr: FsrRenderer | null = null
let fsrUnavailable = false
let fsrRebuilds = 0
let lastRenderPass: RenderPass = 'blit'

function reportFsrFallback(reason: string): void {
  fsrUnavailable = true
  fsr?.dispose()
  fsr = null
  workerScope.postMessage({ type: 'render-fallback', reason })
}

function maybeEmitStats(): void {
  const now = (typeof performance !== 'undefined' ? performance.now() : Date.now())
  const elapsed = now - statsLastEmitMs
  if (elapsed < STATS_EMIT_INTERVAL_MS) return
  const elapsedSec = elapsed / 1000
  const bitrateBps = Math.round((statsBytesInWindow * 8) / elapsedSec)
  const fps = Math.round(statsFramesInWindow / elapsedSec)
  // FR-119 P2b-7 — the window's decode backlog: the mean is what the
  // composable's struggling rule reads, the max is for the HUD.
  const backlogWindow = backlog.takeWindow()
  workerScope.postMessage({
    type: 'stats',
    bitrateBps,
    fps,
    width: statsLastWidth,
    height: statsLastHeight,
    framesDecodedTotal: framesDecoded,
    bytesReceivedTotal: statsBytesTotal,
    decodeQueueSize: decoder?.decodeQueueSize ?? 0,
    backlogMs: round1(backlogWindow.meanMs),
    backlogMaxMs: round1(backlogWindow.maxMs),
    framesDroppedBacklog,
    // FR-17 - the framing counters, surfaced rather than left
    // internal: a rising `chunkStragglers` with steady fps is the
    // unordered transport WORKING (late chunks discarded without
    // harming the frames after them), while a rising `chunkGaps`
    // is real loss costing an IDR each. Told apart only if both
    // are readable - FR-18 shipped a counter nothing read and its
    // acceptance criterion became unmeasurable.
    chunkGaps: framingState.gaps,
    chunkStragglers: framingState.stragglers,
    paint: paintStats.snapshotAndReset(),
    // FR-1 P7 — end-to-end frame age; null until the clock probe lands.
    age: clockOffsetUs !== null ? ageStats.snapshotAndReset() : null,
    // FR-70 M0 — the split: age at ARRIVAL (same clock mapping as `age`)
    // and the local arrival→paint time.
    arrival: clockOffsetUs !== null ? arrivalStats.snapshotAndReset() : null,
    viewer: viewerStats.snapshotAndReset(),
    // FR-59 P3 — window queue growth in ms (null = fewer than two frames
    // arrived, which is no-signal rather than a stable queue).
    queueMs: queueDrift.snapshotAndReset(),
    // FR-135 R3 — the same growth read at arrival (null the same way).
    queueArrMs: arrivalQueueDrift.snapshotAndReset(),
    fwd: fwdStats.snapshotAndReset(),
    decode: decodeStats.snapshotAndReset(),
    outGapMaxMs: round1(outGapMaxMs),
    ctxMode,
    // P7 — the active render path + actual backing size, for the stats
    // pill ("· FSR") and the diag HUD (`fsr@2048x1280`).
    render: lastRenderPass === 'easu-rcas' ? 'fsr' : lastRenderPass === 'rcas' ? 'rcas' : '2d',
    renderW: canvas?.width ?? 0,
    renderH: canvas?.height ?? 0,
    // FR-135 R0 — the window for the agent (`rc-media-window.ts`).
    media: mediaWindow.take(framingState.gaps),
  })
  outGapMaxMs = 0
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

/** FR-17 — reset the byte assembler after a chunk gap. Its header/size
 *  state describes a frame that can no longer be completed, so leaving it
 *  in place would make the next good chunk 0 look like payload. */
function resetAssembler(): void {
  assembler.headerHave = 0
  assembler.payloadBuf = null
  assembler.payloadHave = 0
}

/** FR-17 — per-session, set by `init-canvas`. Default false so an agent
 *  that never advertised `chunk-framing` (or a viewer that didn't ask)
 *  keeps the pre-FR-17 bare byte stream exactly. */
let chunkFraming = false
const framingState = createChunkFraming()

workerScope.onmessage = (e) => {
  const msg = e.data
  if (!msg) return
  if (msg.type === 'clock-cal-ping') {
    // #3566 — answered first, and stamped on arrival: the page measures this
    // worker's clock against its own from the stamp.
    workerScope.postMessage(clockCalPong(msg.seq, epochNowMs()))
  } else if (msg.type === 'init-canvas') {
    if (typeof msg.chunkFraming === 'boolean') chunkFraming = msg.chunkFraming
    canvas = msg.canvas
    // P1 — ctx mode sticks across the idempotent visible-canvas re-init
    // (the watcher's second init-canvas omits it).
    if (msg.ctxMode !== undefined) ctxMode = normalizeCtxMode(msg.ctxMode)
    if (typeof msg.perFrameMsg === 'boolean') perFrameMsg = msg.perFrameMsg
    if (msg.maxBacklogMs !== undefined) {
      maxBacklogMs = normalizeIntKnob(msg.maxBacklogMs, DEFAULT_MAX_BACKLOG_MS, 10, 2000)
    }
    // P7 — FSR knobs stick across the idempotent visible re-init like ctxMode.
    if (msg.sharpen !== undefined) sharpenMode = normalizeSharpenMode(msg.sharpen)
    if (msg.sharpness !== undefined) sharpness = normalizeSharpness(msg.sharpness)
    if (typeof msg.bench === 'boolean') bench.setEnabled(msg.bench)
    ctx = canvas.getContext('2d', ctxOptionsFor(ctxMode))
    if (typeof msg.codec === 'string' && msg.codec.length > 0) {
      activeCodec = msg.codec
    }
    if (
      msg.decodePref === 'prefer-software'
      || msg.decodePref === 'prefer-hardware'
      || msg.decodePref === 'no-preference'
    ) {
      decodePref = msg.decodePref
    }
    // FR-130 P2 — before `initDecoder`, whose configure() reads it.
    if (typeof msg.colorSpace === 'boolean') decoderColorSpace.setWanted(msg.colorSpace)
    // FR-135 R1 — sticky across the visible-canvas re-init, which omits it.
    if (typeof msg.decodeRecover === 'boolean') decodeRecovery.setEnabled(msg.decodeRecover)
    if (typeof msg.unorderedVideo === 'boolean') decodeRecovery.setUnorderedChannel(msg.unorderedVideo)
    initDecoder()
  } else if (msg.type === 'viewport') {
    // P7 — cache the element box; the sizing policy runs per paint.
    viewport = { cssW: msg.cssW, cssH: msg.cssH, dpr: msg.dpr }
  } else if (msg.type === 'clock-offset') {
    clockOffsetUs = typeof msg.offsetUs === 'number' ? msg.offsetUs : null
  } else if (msg.type === 'clock-cal') {
    // #3566 — the skew the page measured, once, when it started this worker.
    pageSkewMs = normalizeSkewMs(msg.skewMs)
  } else if (msg.type === 'media-hold') {
    // FR-135 R0c — the agent announced a capture hold: its gap is no freeze.
    mediaWindow.onHold()
  } else if (msg.type === 'bench-arm') {
    bench.arm(msg.seq)
  } else if (msg.type === 'render-mode') {
    if (msg.sharpen !== undefined) sharpenMode = normalizeSharpenMode(msg.sharpen)
    if (msg.sharpness !== undefined) sharpness = normalizeSharpness(msg.sharpness)
  } else if (msg.type === 'chunk') {
    onChunk(msg.bytes, msg.sentAt)
  } else if (msg.type === 'video-channel') {
    // FR-135 R3 — the page moved the channel itself here, and keeps no copy
    // of it, so it waits to hear the channel arrived (`video-channel-attached`).
    attachVideoChannel(msg.channel)
    workerScope.postMessage({ type: 'video-channel-attached' })
  } else if (msg.type === 'close') {
    teardown()
  }
}

/** FR-135 R3 — a message the browser could not deliver here: it could not be
 *  deserialized in this realm. The only one carrying more than data is the
 *  moved channel, so the page reads this as a move that failed. */
workerScope.onmessageerror = () => {
  workerScope.postMessage({ type: 'message-error' })
}

/** One message off the `video-bytes` channel, however it came: relayed by the
 *  page (`chunk`, stamped with the page's `sentAt`, so the hop is timed) or
 *  straight off the channel the page moved here (FR-135 R3), which has no
 *  page hop to time. */
function onChunk(bytes: ArrayBuffer, sentAt: number | undefined): void {
  framesReceived++
  if (typeof sentAt === 'number') {
    // #3566 — `sentAt` is on the page's clock: read through the skew.
    const fwdMs = fwdHopMs(epochNowMs(), sentAt, pageSkewMs)
    fwdStats.add(fwdMs)
    mediaWindow.onForward(fwdMs)
  }
  const u8 = new Uint8Array(bytes)
  statsBytesInWindow += u8.byteLength
  statsBytesTotal += u8.byteLength
  noteChunkForStallCheck()
  // FR-17 — when the session negotiated framing, every message carries
  // an 8-byte prefix and a gap in it means the frame under assembly can
  // never complete. Feeding its surviving chunks to `consumeBytes`
  // would splice two frames into one bitstream, which the decoder
  // reports as corruption rather than loss, so the assembler is reset
  // and a fresh IDR requested instead.
  if (chunkFraming) {
    const { payload, gap } = stripChunkPrefix(framingState, u8)
    if (gap) {
      resetAssembler()
      // FR-135 R1 — the deltas behind the gap reference the frame it lost,
      // so they wait at the gate for the IDR asked for below. Decoded, they
      // paint garbage until it lands, or fail the decoder. Not where a gap is
      // routine, or with the switch off (`gapClosesGate`).
      if (decodeRecovery.gapClosesGate()) {
        sawKeyframe = false
        mediaWindow.onGateArmed(performance.now())
      }
      workerScope.postMessage({
        type: 'frame-rejected',
        reason: 'chunk-gap',
        gaps: framingState.gaps,
      })
      requestKeyframeResync()
    }
    if (payload) consumeBytes(payload)
  } else {
    consumeBytes(u8)
  }
  maybeEmitStats()
}

/** FR-135 R3 — the `video-bytes` channel the page moved here
 *  (`moveVideoChannelToWorker`), and the delivery-gap clock the page keeps for
 *  a channel it relays: only the worker sees this one's messages. */
let videoChannel: RTCDataChannel | null = null
const videoChannelGap = createDeliveryGapClock()
let videoChannelOpenSaid = false
let videoChannelClosedSaid = false

/** Read `channel` here. Its open and close go to the page
 *  (`video-channel-open`, `video-channel-closed`), which logs them and marks
 *  `dc_open` as it does for a channel it relays, and a silence longer than
 *  `VIDEO_DC_GAP_MS` goes as `video-channel-gap`. The binary type does not
 *  travel with a channel, so it is set here. */
function attachVideoChannel(channel: RTCDataChannel): void {
  detachVideoChannel()
  videoChannel = channel
  channel.binaryType = 'arraybuffer'
  channel.onmessage = (ev: MessageEvent) => {
    if (!(ev.data instanceof ArrayBuffer)) return
    const gapMs = videoChannelGap.note(performance.now())
    if (gapMs !== null) workerScope.postMessage({ type: 'video-channel-gap', gapMs: Math.round(gapMs) })
    onChunk(ev.data, undefined)
  }
  channel.onopen = () => sayVideoChannel('open')
  channel.onclose = () => sayVideoChannel('closed')
  // Moved in the task that made it, a channel arrives connecting. One already
  // past that says so now, because its event will not come again.
  if (channel.readyState === 'open') sayVideoChannel('open')
  else if (channel.readyState === 'closed') sayVideoChannel('closed')
}

/** Tell the page the moved channel opened or closed, once each. */
function sayVideoChannel(state: 'open' | 'closed'): void {
  if (state === 'open') {
    if (videoChannelOpenSaid) return
    videoChannelOpenSaid = true
    workerScope.postMessage({ type: 'video-channel-open' })
  } else {
    if (videoChannelClosedSaid) return
    videoChannelClosedSaid = true
    workerScope.postMessage({ type: 'video-channel-closed' })
  }
}

/** Stop reading the moved channel. It is not closed: a relayed channel stays
 *  open when its path stops too, and the session's `pc.close()` closes either. */
function detachVideoChannel(): void {
  if (videoChannel) {
    videoChannel.onmessage = null
    videoChannel.onopen = null
    videoChannel.onclose = null
    videoChannel = null
  }
  videoChannelGap.reset()
  videoChannelOpenSaid = false
  videoChannelClosedSaid = false
}

/** An error closes a VideoDecoder, so start a new one and wait for a key frame
 *  as after configure() (the leading-delta gate drops the deltas meanwhile and
 *  keeps asking for one). FR-130 P2 calls this when a decoder told the colour
 *  space fails before its first frame (`initDecoder` then configures without
 *  it), and FR-135 R1 when any decoder fails after it. */
function rebuildDecoder(): void {
  try {
    decoder?.close()
  } catch {
    /* already closed by its error */
  }
  decoder = null
  configured = false
  sawKeyframe = false
  // FR-135 R0 (N12) — the picture waits from here (R1's recovery is this case).
  mediaWindow.onGateArmed(performance.now())
  framesSkippedAwaitingKey = 0
  // The failed decoder's submissions will never come out; left in, they
  // would read as a growing backlog and shed the new decoder's deltas.
  backlog.clear()
  pendingArrivalUs.clear()
  initDecoder()
  // Ask for the key frame at once, past `requestKeyframeResync`'s debounce:
  // rebuilds are few (once a session for FR-130, a budget for R1), and early in
  // a worker's life the debounce swallows a request (its clock starts at the
  // worker's creation).
  lastKeyframeReqMs = performance.now()
  workerScope.postMessage({ type: 'request-keyframe' })
}

/** FR-135 R1 — the decoder failed: its error callback (`decoder-error`) or
 *  `decode()` throwing (`decode-error`). `rc-decode-recovery.ts` decides whether
 *  this session replaces the decoder in place. If not, the composable hears the
 *  error as before and takes the path down. */
function onDecoderFailure(type: 'decoder-error' | 'decode-error', err: unknown): void {
  const error = extractErrorMessage(err)
  if (decodeRecovery.onFailure(framesDecoded, performance.now())) {
    workerScope.postMessage({ type: 'decode-recovering', error, attempt: decodeRecovery.attempts })
    rebuildDecoder()
    return
  }
  workerScope.postMessage({ type, error })
}

function initDecoder() {
  if (decoder) return
  decoder = new VideoDecoder({
    output: (frame) => {
      framesDecoded++
      // FR-135 R2 — the first picture means the page is up, so its rAF ticks.
      if (framesDecoded === 1) startViewerHzBursts((hz) => workerScope.postMessage({ type: 'viewer-hz', hz }))
      statsFramesInWindow++
      noteOutputForStallCheck()
      // P1 — submit→output latency (queue + decoder time); FR-119 P2b-7 —
      // the same bookkeeping is the decode backlog the shed rule reads.
      const decodeMs = backlog.output(frame.timestamp, performance.now())
      if (decodeMs !== undefined) {
        decodeStats.add(decodeMs)
        mediaWindow.onDecoded(decodeMs)
      }
      // rc.100/rc.102 — Chrome's NVDEC HEVC decode mis-reports the picture
      // geometry for our hevc_nvenc stream (field DEVBOX, RTX 5090):
      // the agent encodes a FULL 2560×1600 desktop (proven by the FFmpeg DC
      // pump heartbeat: w=2560 h=1600 enc=hevc_nvenc, ~3 MB/window), but the
      // decoded VideoFrame carries a SPURIOUS conformance window —
      // codedWidth/Height = 2560×1600 yet visibleRect/displayWidth = 1280×720
      // at offset (0,0). `drawImage` honours visibleRect, so we were painting
      // only the top-left region. The coded pixels ARE the whole desktop, so
      // re-wrap the frame with visibleRect = the full coded rect and render
      // THAT. (rc.100 first moved to codedWidth for the reported intrinsic;
      // rc.102 also overrides the crop drawImage honours.)
      //
      // A SMALL top-left crop is NOT that bug, though — it is a legitimate
      // encoder alignment crop, and the rewrap must NOT override it. Field
      // WINHOST-F (hevc_qsv, 1920×1080 — the first non-mod-16-height
      // sender): QSV pads HEVC to its row alignment, coding 1920×1088 with a
      // conformance window hiding 8 rows of pooled-surface junk that changes
      // per frame. Rendering the coded rect painted the junk — a purple/blue
      // band flickering on every arriving frame during window drags. Every
      // earlier QSV host had a mod-16 panel (1200/1600/2160 → no crop), which
      // is why qsv looked "unaffected". `classifyCrop` separates the cases.
      //
      // FR-135 R3 — every codec is classified (the class rides the first frame),
      // but only HEVC acts on it. The rule reads a coded size padded by 64 px or
      // more as the NVDEC bug, and Chromium's software VP9 decoder pads every
      // frame that much (libvpx's buffer: 1984×1080 coded for a 1920×1080 picture,
      // measured 2026-11-07), so re-wrapped, a VP9 session would paint the padding.
      // Any other codec paints the picture its decoder says is visible at its
      // natural size, as the VP9 worker did (ledger S19 stays open).
      const vr = frame.visibleRect
      const crop = classifyCrop(frame.codedWidth, frame.codedHeight, vr)
      let render: VideoFrame = frame
      let rewrapped = false
      let renderW = frame.displayWidth
      let renderH = frame.displayHeight
      if (/^(?:hev1|hvc1)\./i.test(activeCodec)) {
        renderW = frame.codedWidth || frame.displayWidth
        renderH = frame.codedHeight || frame.displayHeight
        if (crop === 'spurious') {
          try {
            render = new VideoFrame(frame, { visibleRect: { x: 0, y: 0, width: frame.codedWidth, height: frame.codedHeight } })
            rewrapped = true
          } catch {
            render = frame // re-wrap rejected → fall back to the cropped frame
          }
        } else if (crop === 'alignment' && vr) {
          renderW = vr.width // trust the conformance window: paint and report the visible dims
          renderH = vr.height
        }
      }
      // Capture the one-shot diagnostic BEFORE paintFrame() calls close()
      // (a closed VideoFrame reports 0/null — rc.100 logged {0,0} this way).
      let firstFrameMsg: Record<string, unknown> | null = null
      // rc.187 — post dims on frame 1 AND whenever the decoded size changes
      // (the agent's viewer-adaptive resolution downscales mid-session), so the
      // composable's cursor mapping never divides by a stale intrinsic.
      if (renderW !== lastPostedW || renderH !== lastPostedH) {
        lastPostedW = renderW
        lastPostedH = renderH
        firstFrameMsg = {
          type: 'first-frame',
          width: renderW,
          height: renderH,
          coded: { w: frame.codedWidth, h: frame.codedHeight },
          display: { w: frame.displayWidth, h: frame.displayHeight },
          visible: vr ? { x: vr.x, y: vr.y, w: vr.width, h: vr.height } : null,
          crop,
          rewrapped,
          // FR-130 P2 — the colour space the decoder says it produced: how the
          // field reads, per session, what it made of the stream.
          colorSpace: frameColorSpace(frame),
        }
      }
      statsLastWidth = renderW
      statsLastHeight = renderH
      // FR-1 P7 — capture the wire timestamp BEFORE paint can close the
      // frame; a closed VideoFrame reports 0 (the rc.100 lesson).
      const wireTsUs = frame.timestamp
      // FR-16 L4 — a clone for the probe read, taken before paint closes
      // `render`; null unless a bench probe is outstanding.
      const benchClone = bench.take(render)
      const paintT0 = performance.now()
      const drew = paintFrame(render, renderW, renderH)
      const paintT1 = performance.now()
      paintStats.add(paintT1 - paintT0)
      // FR-135 R0 (N9, N11, N12, N16) — a frame on the canvas.
      if (drew) mediaWindow.onPainted(paintT1, paintT1 - paintT0, wireTsUs)
      // #3566 — the page times the probe from its own clock, so the paint
      // stamp goes back on it.
      if (benchClone) bench.settle(benchClone, toPageEpochMs(epochNowMs(), pageSkewMs))
      // FR-135 R1 — this paint ends an outage, if one is open.
      const recovered = decodeRecovery.onOutput(performance.now())
      if (recovered) {
        workerScope.postMessage({ type: 'decode-recovered', ms: Math.round(recovered.ms), attempts: recovered.attempts })
      }
      // Age at paint on the agent's clock (mapped via the rc:clock probe):
      // framing → send queue → network → decode → paint, end to end. #3566 —
      // the page measured that offset against its own clock, so it is read
      // here through the skew.
      const paintedUs = epochNowUs()
      const agentOffsetUs = clockOffsetUs === null ? null : workerAgentOffsetUs(clockOffsetUs, pageSkewMs)
      if (agentOffsetUs !== null) {
        ageStats.add(frameAgeMs(wireTsUs, agentOffsetUs, paintedUs))
      }
      // FR-70 M0 — the same age at the moment the frame ARRIVED, and the
      // local arrival→paint time; see `arrivalStats`.
      const arrivedUs = pendingArrivalUs.get(wireTsUs)
      if (arrivedUs !== undefined) {
        pendingArrivalUs.delete(wireTsUs)
        if (agentOffsetUs !== null) {
          arrivalStats.add(frameAgeMs(wireTsUs, agentOffsetUs, arrivedUs))
        }
        viewerStats.add((paintedUs - arrivedUs) / 1000)
      }
      // FR-59 P3 — unconditional: the drift needs no probe lock. Fed at paint
      // (see `queueDrift`); `arrivalQueueDrift` is the reading at arrival.
      queueDrift.add(wireTsUs, paintedUs)
      if (rewrapped) frame.close() // paintFrame closed `render`; close the original too
      // rc.187 — report dims on frame 1 AND on every size change (live
      // downscale), so the composable updates the cursor-mapping intrinsic.
      if (firstFrameMsg) workerScope.postMessage(firstFrameMsg)
      if (perFrameMsg && framesDecoded > 1) {
        // P1 — legacy per-frame counter message, OFF by default (a pure
        // main-thread tax at 60 fps); the 1 s stats message carries
        // framesDecodedTotal. localStorage roomler-rc-per-frame-msg
        // restores it for A/B.
        workerScope.postMessage({ type: 'frame-decoded', count: framesDecoded })
      }
      // Emit on every output too so dims update promptly during
      // resolution changes (rc:resolution / DPI flip / monitor swap).
      maybeEmitStats()
    },
    error: (err) => {
      // FR-130 P2 — a decoder told the colour space that fails before its
      // first frame gets one more try without it, so a browser that refuses
      // the member or a value in it still shows the picture. Any other error
      // goes to `onDecoderFailure` (before the first frame the composable bans
      // this transport and renegotiates).
      if (decoderColorSpace.retryAfterError(err, framesDecoded)) {
        rebuildDecoder()
        return
      }
      onDecoderFailure('decoder-error', err)
    },
  })
  // Configure. FR-130 P2 — `rcDecoderConfig` adds the colour space the agent
  // converted with (set S) while the session wants it, and a configure() that
  // throws on it goes again without it (`rc-decoder-config.ts`). Every other
  // field is the worker's own, unchanged. A codec this browser cannot decode
  // after all shows up in the error callback above, and before the first
  // frame the composable bans the transport and renegotiates.
  try {
    const applied = decoderColorSpace.configure(decoder, (colorSpace) => rcDecoderConfig({
      codec: activeCodec,
      optimizeForLatency: true,
      // Hardware-decode preference. `'no-preference'` lets Chromium pick
      // hardware where it genuinely exists and software otherwise, which is
      // the silent fallback we want. `'prefer-hardware'` was tried for VP9
      // profile 1 (added in aac736b) on the theory Chromium would fall back
      // to software on GPUs without 4:4:4 support (profile-1 hardware decode
      // is rare: Intel Tiger Lake+, some AMD; NVDEC and Intel UHD 630 don't
      // expose it), but on Chrome 148 it does NOT: `configure()` accepts the
      // hint, then the first real frame hard-rejects with "Unsupported
      // configuration", the decoder closes, and the canvas goes black (field
      // repro 2026-05-21, RTX 5090 + UHD 630). HEVC has no software decoder in
      // WebCodecs at all, so there it is hardware or nothing: the composable
      // probes `isConfigSupported` before it starts this worker on HEVC, and
      // a driver hiccup mid-session reaches the error callback.
      // 2026-07-24 — `decodePref` (localStorage A/B) can override to
      // prefer-software/-hardware for the decode-stall field diagnosis;
      // defaults to 'no-preference' (unchanged behaviour).
      hardwareAcceleration: decodePref,
    } as VideoDecoderConfig, colorSpace))
    configured = true
    // FR-135 R0 (N9) — and whether the browser has hardware for it.
    mediaWindow.probeDecoderHw(activeCodec, decodePref)
    workerScope.postMessage({
      type: 'decoder-configured',
      codec: activeCodec,
      pref: decodePref,
      // FR-130 P2 — the colour space configure() took, or null (the stream's
      // own description decides).
      colorSpace: applied.colorSpace ?? null,
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
          // Out-of-spec; drop and resync. 16 MB cap is generous: a 4K I444
          // VP9 keyframe at very high bitrate is ~6 MB, a 4K HEVC IDR 0.5-2 MB.
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
  // FR-135 R0 (N10) — the cadence whole frames arrive at, decoder or not.
  mediaWindow.onArrival(performance.now())
  // FR-135 R3 — and the transit queue's growth, read here, at arrival.
  arrivalQueueDrift.add(Number(assembler.pendingTimestampUs), epochNowUs())
  if (!decoder || !configured) return
  const payload = assembler.payloadBuf
  if (!payload) return
  const isKey = (assembler.pendingFlags & FLAG_KEYFRAME) !== 0
  // rc.103 — gate on the first keyframe (see `sawKeyframe` note above).
  // Feeding a leading delta to the decoder throws "A key frame is required"
  // and the composable then tears the whole path down. Drop deltas until the
  // IDR (right behind them) arrives.
  if (!shouldDecodeFrame(sawKeyframe, isKey)) {
    framesSkippedAwaitingKey++
    // 2026-07-24 freeze fix — keep re-requesting the resync IDR while gated.
    // The backlog branch below fires the request exactly ONCE (it re-arms the
    // gate, so it can never fire again for this wedge); if that single
    // control-DC message was lost — or the natural GOP IDR is far away
    // because the viewer-rate cap shed the encode rate (240-frame GOP at
    // 12 fps = 20 s) or the screen went static (frame-counted GOP never
    // advances) — the decoder sat wedged on a stale frame indefinitely
    // (field: "old screen for 30+ s", DEVBOX viewing WINHOST-A). The 250 ms
    // debounce + the agent's MIN_KF_GAP clamp bound this to ~4 req/s.
    requestKeyframeResync()
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
    mediaWindow.onGateOpened(Number(assembler.pendingTimestampUs))
    workerScope.postMessage({
      type: 'keyframe-acquired',
      droppedBefore: framesSkippedAwaitingKey,
    })
  }
  // rc.130 — backlog shed. If the decoder has fallen behind, drop this delta
  // and re-arm the keyframe gate so we keep dropping deltas (which would
  // otherwise decode-error against the missing reference and tear the path
  // down) until the resync IDR lands. FR-119 P2b-7 — "behind" is the backlog
  // in ms, sampled here once per arriving frame (the struggling rule reads the
  // window's mean of these samples).
  const backlogMs = backlog.sample(performance.now())
  mediaWindow.onBacklog(backlogMs)
  if (!isKey && backlogShouldDrop(backlogMs, maxBacklogMs)) {
    framesDroppedBacklog++
    sawKeyframe = false
    mediaWindow.onGateArmed(performance.now())
    requestKeyframeResync()
    if (framesDroppedBacklog === 1 || framesDroppedBacklog % 60 === 0) {
      workerScope.postMessage({
        type: 'backlog-drop',
        dropped: framesDroppedBacklog,
        queue: decoder.decodeQueueSize,
        backlogMs: Math.round(backlogMs),
      })
    }
    return
  }
  // EncodedVideoChunk.timestamp is a microsecond integer per spec.
  // We pass the agent-side framing timestamp through unmodified —
  // VideoDecoder uses it for ordering / frame.timestamp passthrough.
  const ts = Number(assembler.pendingTimestampUs)
  try {
    const chunk = new EncodedVideoChunk({
      type: isKey ? 'key' : 'delta',
      timestamp: ts,
      data: payload,
    })
    // P1 — record the submission time for the submit→output hop; FR-119
    // P2b-7 — and for the decode backlog (bounded inside `DecodeBacklog`).
    backlog.submitted(ts, performance.now())
    // FR-70 M0 — and the frame's arrival (its last chunk is in hand here).
    if (pendingArrivalUs.size > 240) pendingArrivalUs.clear()
    pendingArrivalUs.set(ts, epochNowUs())
    decoder.decode(chunk)
  } catch (err) {
    onDecoderFailure('decode-error', err)
  }
}

/** Draw a decoded frame at `w`×`h`; true when it reached the canvas. */
function paintFrame(frame: VideoFrame, w: number, h: number): boolean {
  if (!canvas || !ctx) {
    frame.close()
    return false
  }
  try {
    // P7 — FSR sharpening upscale (see rc-fsr-render.ts). The sizing policy
    // is pure and ns-cheap, so it runs per paint: a mid-stream resolution
    // flip (the agent rebuilds on rung changes) or a window resize just
    // yields a different target next frame. `blit` is the pre-P7 path,
    // byte-identical; any GL failure falls back to it the SAME frame.
    const target = computeRenderTarget(
      w,
      h,
      viewport?.cssW ?? 0,
      viewport?.cssH ?? 0,
      viewport?.dpr ?? 1,
      fsrUnavailable ? 'off' : sharpenMode,
    )
    let painted = false
    if (target.pass !== 'blit') {
      if (!fsr && !fsrUnavailable) {
        fsr = FsrRenderer.create()
        if (!fsr) reportFsrFallback('webgl2-unavailable')
      }
      if (fsr) {
        const src = fsr.render(frame, w, h, target.w, target.h, target.pass, sharpness)
        if (src) {
          if (canvas.width !== target.w) canvas.width = target.w
          if (canvas.height !== target.h) canvas.height = target.h
          ctx.drawImage(src, 0, 0, target.w, target.h)
          lastRenderPass = target.pass
          painted = true
        } else {
          // Lost/failed GL — dispose so the next frame lazily re-creates a
          // fresh context; after MAX_FSR_REBUILDS the session goes 2D.
          fsr.dispose()
          fsr = null
          fsrRebuilds++
          if (fsrRebuilds > MAX_FSR_REBUILDS) reportFsrFallback('gl-context-lost')
        }
      }
    }
    if (!painted) {
      if (canvas.width !== w) canvas.width = w
      if (canvas.height !== h) canvas.height = h
      // rc.100 — pass the explicit dest rect. The bare `drawImage(frame, 0, 0)`
      // uses the frame's `displayWidth/Height` as its natural size, which is
      // exactly the shrunken value we're routing around on the NVDEC-bug path;
      // an explicit dest of the render size (coded for the rewrapped spurious
      // case, visible for a trusted alignment crop) keeps the output 1:1.
      ctx.drawImage(frame, 0, 0, w, h)
      lastRenderPass = 'blit'
    }
    return true
  } catch {
    /* canvas lost mid-teardown */
    return false
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
  // FR-135 R3 — a moved channel stops being read with the session.
  detachVideoChannel()
  framesDecoded = 0
  framesReceived = 0
  lastPostedW = 0
  lastPostedH = 0
  // rc.103 — re-arm the leading-delta gate so a fresh decoder (re-connect
  // / re-negotiation) again waits for an IDR before its first decode().
  sawKeyframe = false
  framesSkippedAwaitingKey = 0
  framesDroppedBacklog = 0
  // P1 — drop the hop-instrumentation state with the session.
  backlog.clear()
  pendingArrivalUs.clear()
  paintStats.snapshotAndReset()
  fwdStats.snapshotAndReset()
  decodeStats.snapshotAndReset()
  ageStats.snapshotAndReset()
  arrivalStats.snapshotAndReset()
  viewerStats.snapshotAndReset()
  queueDrift.snapshotAndReset()
  queueDrift.reset()
  arrivalQueueDrift.snapshotAndReset()
  arrivalQueueDrift.reset()
  clockOffsetUs = null
  pageSkewMs = 0
  outGapMaxMs = 0
  mediaWindow.reset()
  // P7 — release the GL renderer; a fresh session re-creates lazily and
  // gets a fresh unavailability verdict (a new tab/GPU state may differ).
  fsr?.dispose()
  fsr = null
  fsrUnavailable = false
  fsrRebuilds = 0
  lastRenderPass = 'blit'
  viewport = null
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

/** rc.103 — leading-delta gate decision, pure for vitest. Returns whether
 *  a frame should be fed to `decoder.decode()`: a delta is only decodable
 *  once a keyframe has already been seen; a keyframe is always decodable.
 *  This is the guard that stops the decoder throwing "A key frame is
 *  required after configure() or flush()" on a leading delta — the field
 *  WINHOST-G hevc_qsv failure (async-encoder DC-open race). */
export function shouldDecodeFrame(hasSeenKeyframe: boolean, isKey: boolean): boolean {
  return hasSeenKeyframe || isKey
}

/** Crop classification for a decoded frame, any codec — pure for vitest.
 *
 *  - `exact`: no conformance window (visibleRect == coded size), or the
 *    frame carries no usable geometry — render as-is.
 *  - `alignment`: a REAL encoder alignment crop — origin (0,0) and less
 *    than one CTU (64 px) smaller than the coded size on BOTH axes. HEVC
 *    and H.264 encoders pad the picture to their block alignment and crop
 *    the excess: Intel QSV codes a 1920×1080 desktop as 1920×1088 with an
 *    8-row bottom crop whose padding rows are pooled-surface junk. TRUST it —
 *    `drawImage` honours the visibleRect natively. (Field WINHOST-F: painting
 *    the coded rect instead showed the junk as a purple/blue band flickering
 *    on every frame while dragging windows.)
 *  - `spurious`: anything else — a drastically smaller visible rect or an
 *    offset origin is the Chrome NVDEC misreported-geometry bug (field
 *    DEVBOX: coded 2560×1600, visible 1280×720). The coded pixels
 *    are the whole desktop → re-wrap and render the full coded rect.
 *
 *  An alignment pad can never reach 64: aligning to any of 8/16/32/64 pads
 *  by at most alignment−1 px, so a ≥64 deficit means the visible picture is
 *  genuinely smaller than the coded surface — NVDEC-bug territory. */
export function classifyCrop(
  codedW: number,
  codedH: number,
  vr: { x: number; y: number; width: number; height: number } | null | undefined,
): 'exact' | 'alignment' | 'spurious' {
  if (!vr || codedW <= 0 || codedH <= 0) return 'exact'
  if (vr.width === codedW && vr.height === codedH) return 'exact'
  const dw = codedW - vr.width
  const dh = codedH - vr.height
  if (vr.x === 0 && vr.y === 0 && dw >= 0 && dh >= 0 && dw < 64 && dh < 64) {
    return 'alignment'
  }
  return 'spurious'
}
