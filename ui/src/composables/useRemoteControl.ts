import { ref, watch, onBeforeUnmount } from 'vue'
import { useWsStore } from '@/stores/ws'
import { api } from '@/api/client'

/**
 * Remote-control session state machine driven from the controller browser.
 *
 * Lifecycle: idle → requesting → awaiting_consent → negotiating → connected
 *                                                                ↘ error
 *                                                                ↘ closed
 *
 * The composable owns one RTCPeerConnection per session. It uses the shared
 * WS connection (useWsStore) as the signalling transport and speaks the
 * `rc:*` protocol. See docs/remote-control.md §7.
 */

export type RcPhase =
  | 'idle'
  | 'requesting'
  | 'awaiting_consent'
  | 'negotiating'
  | 'connected'
  | 'closed'
  | 'error'

/** Controller's quality preference. `auto` lets the agent follow TWCC; `low`
 *  clamps for bandwidth-constrained WAN; `high` asks for the best codec the
 *  agent can offer (HEVC/AV1 when negotiated in Phase 2). Persisted to
 *  `localStorage` so it survives a page reload. */
export type RcQuality = 'auto' | 'low' | 'high'

/** Live readout of the inbound video stream derived from
 *  `RTCPeerConnection.getStats()`. Updated every 500 ms while connected. */
export interface RcStats {
  /** Decoded inbound bitrate in bits per second. 0 until two polls land. */
  bitrate_bps: number
  /** Decoded framerate reported by the browser. */
  fps: number
  /** Codec short name ("H264", "H265", "AV1", "VP9", "VP8"). Empty string
   *  until the browser reports one. Phase 2 uses this for the UI badge. */
  codec: string
}

/** Remote cursor state: position in source pixels + a cache of shape
 *  bitmaps keyed by the handle id the agent sent. RemoteControl.vue
 *  uses this to draw the real OS cursor over the video (replacing the
 *  synthetic initials badge for single-controller sessions). Undefined
 *  while the agent hasn't advertised — the view falls back to the
 *  initials badge. */
export interface RcCursor {
  /** Current position in agent-source pixels. Null = hidden
   *  (fullscreen video, cursor moved off primary display). */
  pos: { x: number; y: number; id: number } | null
  /** ImageBitmap cache by shape id. Pure side-effect: decoding a
   *  shape on receive means the paint loop can hand it straight to
   *  canvas.drawImage without per-frame decode cost. */
  shapes: Map<number, { bitmap: ImageBitmap; hotspotX: number; hotspotY: number }>
}

interface IceServer {
  urls: string[]
  username?: string
  credential?: string
}

interface TurnCredsResponse {
  ice_servers: IceServer[]
}

const EMPTY_STATS: RcStats = { bitrate_bps: 0, fps: 0, codec: '' }
const QUALITY_STORAGE_KEY = 'rc:quality'
const STATS_POLL_MS = 500

function readStoredQuality(): RcQuality {
  try {
    const v = globalThis.localStorage?.getItem(QUALITY_STORAGE_KEY)
    if (v === 'low' || v === 'high' || v === 'auto') return v
  } catch {
    /* localStorage may be disabled or unavailable (SSR). */
  }
  return 'auto'
}

function persistQuality(q: RcQuality) {
  try {
    globalThis.localStorage?.setItem(QUALITY_STORAGE_KEY, q)
  } catch {
    /* best-effort — swallow quota / privacy-mode errors */
  }
}

/** Persist key for the optional codec override. When the user forces a
 *  specific codec (H.265 or AV1) for an A/B comparison, we save it so
 *  the choice survives a page reload. `null` means no override — the
 *  agent picks from the full browser×agent intersection. */
const PREFERRED_CODEC_STORAGE_KEY = 'roomler-rc-preferred-codec'

/** Codec names that round-trip between `RTCRtpReceiver.getCapabilities`,
 *  the agent's advertised caps, and SDP fmtp munging. Keep in sync
 *  with `encode/caps.rs::pick_best_codec`. */
export type RcPreferredCodec = 'h264' | 'h265' | 'av1' | 'vp9' | 'vp8'

function readStoredPreferredCodec(): RcPreferredCodec | null {
  try {
    const raw = globalThis.localStorage?.getItem(PREFERRED_CODEC_STORAGE_KEY)
    if (raw === 'h264' || raw === 'h265' || raw === 'av1' || raw === 'vp9' || raw === 'vp8') {
      return raw
    }
  } catch {
    /* privacy mode → treat as no override */
  }
  return null
}

function persistPreferredCodec(c: RcPreferredCodec | null) {
  try {
    if (c == null) {
      globalThis.localStorage?.removeItem(PREFERRED_CODEC_STORAGE_KEY)
    } else {
      globalThis.localStorage?.setItem(PREFERRED_CODEC_STORAGE_KEY, c)
    }
  } catch {
    /* best-effort */
  }
}

/** How the remote video is rendered inside the viewer stage.
 *  - `adaptive`: fit-to-stage with aspect preserved (default;
 *    equivalent to `object-fit: contain`).
 *  - `original`: 1:1 intrinsic pixels; stage shows scrollbars if the
 *    remote is larger than the viewport.
 *  - `custom`: scaled to `scaleCustomPercent` of intrinsic size. */
export type RcScaleMode = 'adaptive' | 'original' | 'custom'

/** Which capture/encode resolution the REMOTE agent should use.
 *  - `original`: the agent's native monitor resolution.
 *  - `fit`: the agent downscales to match the local viewer's stage
 *    dimensions × devicePixelRatio (re-emitted on viewport resize).
 *  - `custom`: an explicit width × height picked from a preset list or
 *    typed in by the operator. */
export type RcResolutionMode = 'original' | 'fit' | 'custom'

export interface RcResolutionSetting {
  mode: RcResolutionMode
  /** Only meaningful for `fit` + `custom`. */
  width?: number
  height?: number
}

/** Per-agent localStorage prefix — resolution preferences should NOT
 *  bleed across machines (a "Fit to local at 1920×1080" set for my
 *  laptop monitor is wrong for my 4K desktop). */
const RESOLUTION_STORAGE_PREFIX = 'roomler-rc-resolution:'

const SCALE_MODE_STORAGE_KEY = 'roomler-rc-scale-mode'
const SCALE_CUSTOM_PCT_STORAGE_KEY = 'roomler-rc-scale-pct'

function readStoredScaleMode(): RcScaleMode {
  try {
    const raw = globalThis.localStorage?.getItem(SCALE_MODE_STORAGE_KEY)
    if (raw === 'adaptive' || raw === 'original' || raw === 'custom') return raw
  } catch {
    /* privacy mode → default */
  }
  return 'adaptive'
}

function persistScaleMode(m: RcScaleMode) {
  try {
    globalThis.localStorage?.setItem(SCALE_MODE_STORAGE_KEY, m)
  } catch {
    /* best-effort */
  }
}

function readStoredScalePct(): number {
  try {
    const raw = globalThis.localStorage?.getItem(SCALE_CUSTOM_PCT_STORAGE_KEY)
    if (raw != null) {
      const n = Number(raw)
      if (Number.isFinite(n) && n >= 5 && n <= 1000) return n
    }
  } catch {
    /* privacy mode → default */
  }
  return 100
}

function persistScalePct(n: number) {
  try {
    globalThis.localStorage?.setItem(SCALE_CUSTOM_PCT_STORAGE_KEY, String(n))
  } catch {
    /* best-effort */
  }
}

function readStoredResolution(agentId: string): RcResolutionSetting {
  try {
    const raw = globalThis.localStorage?.getItem(RESOLUTION_STORAGE_PREFIX + agentId)
    if (raw) {
      const parsed = JSON.parse(raw)
      if (
        parsed &&
        (parsed.mode === 'original' || parsed.mode === 'fit' || parsed.mode === 'custom')
      ) {
        return {
          mode: parsed.mode,
          width: typeof parsed.width === 'number' ? parsed.width : undefined,
          height: typeof parsed.height === 'number' ? parsed.height : undefined,
        }
      }
    }
  } catch {
    /* fall through to default */
  }
  return { mode: 'original' }
}

function persistResolution(agentId: string, s: RcResolutionSetting) {
  try {
    globalThis.localStorage?.setItem(
      RESOLUTION_STORAGE_PREFIX + agentId,
      JSON.stringify(s),
    )
  } catch {
    /* best-effort */
  }
}

/** Translate an `RcResolutionSetting` into the exact JSON shape the
 *  agent's control-DC handler expects. Returns `null` when the
 *  setting is invalid (fit/custom with no dims) — the caller drops
 *  the send rather than emitting a half-formed message. Exported
 *  for tests so the wire format is locked. */
export function resolutionWireMessage(
  s: RcResolutionSetting,
): Record<string, unknown> | null {
  if (s.mode === 'original') {
    return { t: 'rc:resolution', mode: 'original' }
  }
  // fit + custom both require positive integer dims. Missing or
  // zero/negative values return null so the caller drops the send
  // rather than emitting an invalid message.
  if (s.width == null || s.height == null) return null
  if (!Number.isFinite(s.width) || !Number.isFinite(s.height)) return null
  const w = Math.round(s.width)
  const h = Math.round(s.height)
  if (w < 1 || h < 1) return null
  return { t: 'rc:resolution', mode: s.mode, width: w, height: h }
}

/** Which render path the viewer uses for the inbound video track.
 *  - `video`: classic `<video>` element bound to a MediaStream. Goes
 *    through Chrome's built-in jitter buffer (~80 ms soft floor).
 *  - `webcodecs`: a Web Worker receives encoded RTP frames via
 *    `RTCRtpScriptTransform`, decodes them with `VideoDecoder`, and
 *    paints the results to an `OffscreenCanvas`. Bypasses the jitter
 *    buffer for measurable latency savings. Chrome-only in practice;
 *    falls back to `video` when `RTCRtpScriptTransform` or
 *    `VideoDecoder` are unavailable. Takes effect on the next
 *    `connect()` — live sessions keep whatever path they started
 *    with, since swapping receiver transforms mid-session tears
 *    down the decoder. */
export type RcRenderPath = 'video' | 'webcodecs'

const RENDER_PATH_STORAGE_KEY = 'roomler-rc-render-path'

function readStoredRenderPath(): RcRenderPath {
  try {
    const raw = globalThis.localStorage?.getItem(RENDER_PATH_STORAGE_KEY)
    if (raw === 'webcodecs' || raw === 'video') return raw
  } catch {
    /* privacy mode → default */
  }
  return 'video'
}

function persistRenderPath(p: RcRenderPath) {
  try {
    globalThis.localStorage?.setItem(RENDER_PATH_STORAGE_KEY, p)
  } catch {
    /* best-effort */
  }
}

/** Feature-detect WebCodecs + RTCRtpScriptTransform. Returns true only
 *  when both pieces are present — Firefox has VideoDecoder but exposes
 *  insertable streams via a different API, so the toggle stays off
 *  there until we add that path too. Exported for vitest. */
export function isWebCodecsSupported(): boolean {
  const g = globalThis as unknown as {
    RTCRtpScriptTransform?: unknown
    VideoDecoder?: unknown
  }
  return typeof g.RTCRtpScriptTransform === 'function'
    && typeof g.VideoDecoder === 'function'
}

/** Short codec name to pass into `new RTCRtpScriptTransform(worker,
 *  { codec })`. Reads the first negotiated codec off
 *  `RTCRtpReceiver.getParameters().codecs` and maps it back to our
 *  protocol's short name. Defaults to 'h264' when nothing has
 *  negotiated yet or the mime type is unrecognised. Exported for tests. */
export function shortCodecFromReceiver(
  receiver: Pick<RTCRtpReceiver, 'getParameters'> | null | undefined,
): RcPreferredCodec {
  if (!receiver) return 'h264'
  let mime = ''
  try {
    const params = receiver.getParameters()
    const codecs = (params as { codecs?: Array<{ mimeType?: string }> }).codecs
    if (codecs && codecs.length > 0 && codecs[0]?.mimeType) {
      mime = codecs[0].mimeType.toLowerCase()
    }
  } catch {
    return 'h264'
  }
  if (mime.includes('h265') || mime.includes('hevc')) return 'h265'
  if (mime.includes('av1')) return 'av1'
  if (mime.includes('vp9')) return 'vp9'
  if (mime.includes('vp8')) return 'vp8'
  return 'h264'
}

/** Inspect the negotiated codec by reading the remote SDP answer.
 *  More reliable than `RTCRtpReceiver.getParameters()` at
 *  `pc.ontrack` time — Chrome populates that lazily and it's often
 *  empty on first read, which silently defaulted us to H.264 even
 *  when HEVC was negotiated. The SDP, in contrast, is fully settled
 *  by the time ontrack fires (it fires as a consequence of SRD).
 *
 *  Parses the first video m-line's first payload type, then finds
 *  the matching a=rtpmap entry. Returns `null` when nothing could
 *  be parsed — the caller falls back to the receiver-based detector.
 *  Exported for tests so the parse rule is locked. */
export function codecFromSdp(sdp: string | null | undefined): RcPreferredCodec | null {
  if (!sdp) return null
  const lines = sdp.split(/\r?\n/)
  let videoPt: string | null = null
  for (const line of lines) {
    if (line.startsWith('m=video')) {
      // m=video <port> <proto> <pt1> <pt2> ...
      const parts = line.split(' ')
      videoPt = parts[3] ?? null
      break
    }
  }
  if (!videoPt) return null
  const rtpmapPrefix = `a=rtpmap:${videoPt} `
  for (const line of lines) {
    if (!line.startsWith(rtpmapPrefix)) continue
    // a=rtpmap:<pt> <codec>/<rate>[/<params>]
    const rest = line.slice(rtpmapPrefix.length).trim()
    const codec = (rest.split('/')[0] ?? '').toLowerCase()
    switch (codec) {
      case 'h264':
        return 'h264'
      case 'h265':
      case 'hevc':
        return 'h265'
      case 'av1':
      case 'av1x':
        return 'av1'
      case 'vp9':
        return 'vp9'
      case 'vp8':
        return 'vp8'
      default:
        return null
    }
  }
  return null
}

/** Given the full set of browser-supported codecs and an optional
 *  override, return the list the agent should see in `browser_caps`.
 *  When `preferred` is set, only that codec (plus H.264 as a safety
 *  fallback if the browser has it) is forwarded — so the agent's
 *  `pick_best_codec` can only land on the preferred one, or fall back
 *  to H.264 if the agent itself lacks support. Exported for tests. */
export function filterCapsByPreference(
  caps: string[],
  preferred: RcPreferredCodec | null,
): string[] {
  if (preferred == null) return caps
  const out = caps.filter((c) => c === preferred)
  // Always keep H.264 as a parachute — if the user forces AV1 but the
  // agent on this host can't encode AV1, we want a working session
  // rather than a failed one.
  if (preferred !== 'h264' && caps.includes('h264')) {
    out.push('h264')
  }
  return out
}

export function useRemoteControl() {
  const ws = useWsStore()
  const phase = ref<RcPhase>('idle')
  const error = ref<string | null>(null)
  const sessionId = ref<string | null>(null)
  const remoteStream = ref<MediaStream | null>(null)
  /** Set once we've received at least one video/audio track. False until
   *  the agent attaches media (the native agent currently does not). */
  const hasMedia = ref(false)
  /** Live inbound-RTP stats: bitrate, fps, codec. Zero until the first
   *  two polls land (we need two snapshots to derive bitrate). */
  const stats = ref<RcStats>({ ...EMPTY_STATS })
  /** Remote cursor state. `pos` = null → hide the overlay + fall back
   *  to the initials badge. Shape bitmaps are cached so the canvas
   *  paint is just a `drawImage`. */
  const cursor = ref<RcCursor>({ pos: null, shapes: new Map() })
  /** Controller's quality preference, persisted in localStorage. Sent to
   *  the agent over the `control` data channel whenever the user changes
   *  it *or* the channel first opens. */
  const quality = ref<RcQuality>(readStoredQuality())
  /** Optional codec override. `null` = let the agent pick from the full
   *  intersection; `'h265'` = only advertise H.265 + H.264 fallback to
   *  the agent so AV1 can't win. Useful for A/B comparisons
   *  ("is HEVC actually better than H.264 on this link?"). Persisted
   *  to localStorage so the choice survives a page reload. */
  const preferredCodec = ref<RcPreferredCodec | null>(readStoredPreferredCodec())
  /** How the remote video is rendered inside the viewer stage. See
   *  `RcScaleMode`. Persisted per-browser in localStorage. */
  const scaleMode = ref<RcScaleMode>(readStoredScaleMode())
  /** Percent for `scaleMode === 'custom'`. Range 5-1000, clamped at
   *  read/write time. */
  const scaleCustomPercent = ref<number>(readStoredScalePct())
  /** Remote capture/encode resolution choice. Persisted per-agent
   *  (keyed on `agentId` after `connect()`). Starts at `{mode:'original'}`
   *  and narrows when `connect()` supplies the real agent id. */
  const resolution = ref<RcResolutionSetting>({ mode: 'original' })
  // Tracks the last agentId we loaded + persist under. Set in connect().
  let resolutionAgentId: string | null = null

  /** Viewer render path. `video` goes through `<video>` + the browser's
   *  jitter buffer; `webcodecs` uses the Worker + VideoDecoder + canvas
   *  path that bypasses it. Persisted per-browser; defaults to `video`
   *  so the feature stays opt-in while we bed it in. */
  const renderPath = ref<RcRenderPath>(readStoredRenderPath())
  /** Whether this browser actually supports the WebCodecs path. UI
   *  reads this to disable the toggle when the APIs aren't present
   *  (Firefox, Safari < 17, old Chromium). */
  const webcodecsSupported = ref<boolean>(isWebCodecsSupported())
  /** The `<canvas>` the view renders into when `renderPath === 'webcodecs'`
   *  and the session is active. The view writes this ref on mount; the
   *  composable reads it in `pc.ontrack` to transfer control to the
   *  worker. Null in `video` mode. */
  const webcodecsCanvasEl = ref<HTMLCanvasElement | null>(null)
  /** Unified intrinsic dimensions of the rendered remote frame. Driven
   *  by `<video>.onresize` in classic mode and by worker `first-frame`
   *  messages in webcodecs mode. The view reads this for `custom`/`original`
   *  scale styling + input coord math — one source of truth that works
   *  across both paths. */
  const mediaIntrinsicW = ref(0)
  const mediaIntrinsicH = ref(0)
  // WebCodecs runtime handles. Created on track-attach, destroyed in
  // teardown(). Tracked here rather than scoped inside ontrack so
  // teardown() can reliably stop the worker on disconnect.
  let webcodecsWorker: Worker | null = null
  let webcodecsActive = false

  let pc: RTCPeerConnection | null = null
  /** Data channels we open proactively (per docs §5). Labels match the
   *  agent's expected routing: input/control/clipboard/files. */
  const channels: Record<string, RTCDataChannel> = {}
  const inputChannelOpen = ref(false)

  // Pending clipboard:read requests. Keyed by `req_id` so interleaved
  // reads can resolve independently. The agent echoes the req_id back
  // on `clipboard:content` / `clipboard:error`; a 5 s timeout rejects
  // stale requests so the UI toast doesn't spin forever.
  const pendingClipboardReads = new Map<
    number,
    { resolve: (text: string) => void; reject: (err: Error) => void; timer: ReturnType<typeof setTimeout> }
  >()
  let nextClipboardReqId = 1

  // Stats polling: interval handle + last snapshot so each poll can
  // derive a delta bitrate. Reset in teardown() so a fresh connection
  // doesn't see a stale byte counter.
  let statsTimer: ReturnType<typeof setInterval> | null = null
  let statsPrevBytes = 0
  let statsPrevTsMs = 0

  // Coalesce rapid mouse moves to one per animation frame (~60 Hz). Keys
  // and clicks are NOT coalesced — they're too meaningful to drop.
  let pendingMove: { x: number; y: number; mon: number } | null = null
  let rafHandle: number | null = null

  function flushPendingMove() {
    rafHandle = null
    if (!pendingMove || !channels.input || channels.input.readyState !== 'open') return
    sendInput({ t: 'mouse_move', ...pendingMove })
    pendingMove = null
  }

  function sendInput(msg: Record<string, unknown>) {
    const ch = channels.input
    if (!ch || ch.readyState !== 'open') return
    try {
      ch.send(JSON.stringify(msg))
    } catch {
      /* channel may have closed between the check and send — drop */
    }
  }

  /** Send a `rc:quality` preference over the control channel. Safe to
   *  call while the channel is closed — it's a no-op until open. Also
   *  sent automatically when the channel first opens so the agent
   *  learns the restored preference without user interaction. */
  function sendQualityPreference() {
    const ch = channels.control
    if (!ch || ch.readyState !== 'open') return
    try {
      ch.send(JSON.stringify({ t: 'rc:quality', quality: quality.value }))
    } catch {
      /* channel closed between check and send — drop */
    }
  }

  /** Update the controller's quality preference, persist it, and push
   *  the new value to the agent. No-ops (other than the persist) if the
   *  control channel isn't open yet — the onopen handler will re-send. */
  function setQuality(q: RcQuality) {
    quality.value = q
    persistQuality(q)
    sendQualityPreference()
  }

  /** Force a specific codec for the next session. Pass `null` to clear
   *  the override. Takes effect on the next `connect()` — live sessions
   *  keep whatever SDP they negotiated at start. Persisted to
   *  localStorage so the preference survives a reload. */
  function setPreferredCodec(c: RcPreferredCodec | null) {
    preferredCodec.value = c
    persistPreferredCodec(c)
  }

  /** Update the stage render mode. Takes effect immediately — CSS
   *  bindings + input coordinate mapping both switch live. */
  function setScaleMode(m: RcScaleMode) {
    scaleMode.value = m
    persistScaleMode(m)
  }

  /** Update the custom-scale percent (clamped to [5, 1000]). Takes
   *  effect immediately even when `scaleMode !== 'custom'`; switching
   *  back to custom picks up the latest value. */
  function setScaleCustomPercent(n: number) {
    const clamped = Math.round(Math.max(5, Math.min(1000, n)))
    scaleCustomPercent.value = clamped
    persistScalePct(clamped)
  }

  /** Send the current resolution preference over the control DC.
   *  Safe to call while the channel is closed — no-op until open; the
   *  `channels.control.onopen` handler calls this automatically so a
   *  page reload re-emits the stored preference without user action. */
  function sendResolutionPreference() {
    const ch = channels.control
    if (!ch || ch.readyState !== 'open') return
    const msg = resolutionWireMessage(resolution.value)
    if (!msg) return
    try {
      ch.send(JSON.stringify(msg))
    } catch {
      /* channel closed between check and send — drop */
    }
  }

  /** Update the controller's remote-resolution preference and push to
   *  the agent. For `fit` + `custom`, `width`/`height` are required;
   *  for `original`, they're ignored. Persisted per-agent so the
   *  choice survives reloads without bleeding across machines. */
  function setResolution(next: RcResolutionSetting) {
    resolution.value = next
    if (resolutionAgentId) persistResolution(resolutionAgentId, next)
    sendResolutionPreference()
  }

  /** Switch render path. Only takes effect on the next `connect()` —
   *  switching mid-session would require tearing down the receiver
   *  transform and replacing the DOM element the video paints into,
   *  which is more disruption than "reconnect to apply". Persisted
   *  per-browser. If WebCodecs isn't supported on this browser and
   *  the caller asks for `webcodecs`, we clamp to `video` silently so
   *  a stored preference from a different browser doesn't brick the
   *  viewer. */
  function setRenderPath(p: RcRenderPath) {
    const next = p === 'webcodecs' && !webcodecsSupported.value ? 'video' : p
    renderPath.value = next
    persistRenderPath(next)
  }

  /** Install the receiver transform EAGERLY (at pc.ontrack time) so
   *  Chrome routes encoded frames to the worker from the very first
   *  RTP packet. The worker decodes into a null sink until a canvas
   *  arrives via `attachCanvasToWorker()`; once the canvas lands, it
   *  transfers control + the worker starts painting. Previously we
   *  waited for the canvas before installing the transform, which
   *  looked like a race — some Chrome builds seem to lock frames
   *  onto the default decoder when the transform is assigned after
   *  the track has already started producing. */
  function installWebCodecsTransform(receiver: RTCRtpReceiver): boolean {
    const g = globalThis as unknown as {
      RTCRtpScriptTransform?: new (worker: Worker, opts: unknown) => unknown
    }
    const TransformCtor = g.RTCRtpScriptTransform
    if (typeof TransformCtor !== 'function') return false
    let worker: Worker
    try {
      worker = new Worker(
        new URL('../workers/rc-webcodecs-worker.ts', import.meta.url),
        { type: 'module' },
      )
    } catch (err) {
      console.warn('[rc] worker construction failed', err)
      return false
    }
    worker.onmessage = (ev) => {
      const msg = ev.data as Record<string, unknown>
      if (!msg || typeof msg.type !== 'string') return
      if (msg.type === 'first-frame' && typeof msg.width === 'number' && typeof msg.height === 'number') {
        mediaIntrinsicW.value = msg.width
        mediaIntrinsicH.value = msg.height
        console.info('[rc] webcodecs first frame', msg.width, 'x', msg.height)
      } else if (msg.type === 'transform-active') {
        console.info('[rc] webcodecs transform active', msg)
      } else if (msg.type === 'first-encoded-frame' || msg.type === 'early-encoded-frame') {
        console.info('[rc] webcodecs encoded frame', msg)
      } else if (msg.type === 'reader-heartbeat') {
        console.info('[rc] webcodecs heartbeat', msg)
      } else if (msg.type === 'watchdog') {
        console.warn('[rc] webcodecs watchdog fired', msg)
      } else if (
        msg.type === 'decoder-error'
        || msg.type === 'decoder-configure-error'
        || msg.type === 'decode-error'
        || msg.type === 'reader-error'
        || msg.type === 'pipe-error'
      ) {
        console.warn('[rc] webcodecs worker error', msg)
      }
    }
    const sdpCodec = codecFromSdp(pc?.currentRemoteDescription?.sdp)
    const receiverCodec = shortCodecFromReceiver(receiver)
    const codec = sdpCodec ?? receiverCodec
    console.info('[rc] webcodecs path activating; codec:', codec, '(sdp:', sdpCodec, ' receiver:', receiverCodec, ')')
    try {
      ;(receiver as unknown as { transform: unknown }).transform = new TransformCtor(
        worker,
        { codec },
      )
    } catch (err) {
      console.warn('[rc] setting receiver.transform failed', err)
      worker.terminate()
      return false
    }
    webcodecsWorker = worker
    webcodecsActive = true
    // If the canvas is already mounted, attach it now; otherwise
    // the watcher picks it up when it lands.
    if (webcodecsCanvasEl.value) {
      attachCanvasToWorker(webcodecsCanvasEl.value)
    }
    // Kick a getStats diagnostic to confirm whether RTP is actually
    // flowing to this receiver — if bytesReceived rises but the
    // worker never posts `first-encoded-frame`, Chrome is dropping
    // frames before the transform.
    scheduleInboundRtpDiagnostic(receiver)
    return true
  }

  /** Hand an OffscreenCanvas to the worker so it can start painting.
   *  Returns false on transfer failure. Called immediately from
   *  `installWebCodecsTransform` when the canvas is already there,
   *  or later from the `webcodecsCanvasEl` watcher. */
  function attachCanvasToWorker(canvasEl: HTMLCanvasElement): boolean {
    if (!webcodecsWorker) return false
    let offscreen: OffscreenCanvas
    try {
      offscreen = canvasEl.transferControlToOffscreen()
    } catch (err) {
      console.warn('[rc] transferControlToOffscreen failed', err)
      return false
    }
    try {
      webcodecsWorker.postMessage({ type: 'init-canvas', canvas: offscreen }, [offscreen])
      console.info('[rc] webcodecs: canvas attached to worker')
      return true
    } catch (err) {
      console.warn('[rc] worker init-canvas post failed', err)
      return false
    }
  }

  function scheduleInboundRtpDiagnostic(receiver: RTCRtpReceiver) {
    let ticks = 0
    const interval = setInterval(async () => {
      ticks += 1
      if (ticks > 5 || !webcodecsActive) {
        clearInterval(interval)
        return
      }
      try {
        const stats = await receiver.getStats()
        stats.forEach((r: { type?: string; bytesReceived?: number; framesReceived?: number; framesDecoded?: number }) => {
          if (r.type === 'inbound-rtp') {
            console.info('[rc] webcodecs diag inbound-rtp', {
              tick: ticks,
              bytesReceived: r.bytesReceived,
              framesReceived: r.framesReceived,
              framesDecoded: r.framesDecoded,
            })
          }
        })
      } catch { /* ignore */ }
    }, 1000)
  }

  function stopWebCodecsPath() {
    if (!webcodecsWorker) return
    try { webcodecsWorker.postMessage({ type: 'close' }) } catch { /* ignore */ }
    try { webcodecsWorker.terminate() } catch { /* ignore */ }
    webcodecsWorker = null
    webcodecsActive = false
  }

  // Late-canvas watcher. The transform is installed eagerly in
  // pc.ontrack, but the canvas is gated on phase === 'connected'
  // so it mounts after ontrack fires. When it mounts, hand the
  // OffscreenCanvas to the already-running worker so it can start
  // painting what it's been decoding.
  watch(webcodecsCanvasEl, (el) => {
    if (!el || !webcodecsWorker) return
    attachCanvasToWorker(el)
  })

  function startStatsPoll() {
    if (statsTimer !== null) return
    statsTimer = setInterval(async () => {
      if (!pc) return
      try {
        const report = await pc.getStats()
        const snap = extractStatsSnapshot(report, statsPrevBytes, statsPrevTsMs)
        stats.value = snap.next
        statsPrevBytes = snap.bytes
        statsPrevTsMs = snap.tsMs
      } catch {
        /* getStats() can reject during teardown — just wait for next tick */
      }
    }, STATS_POLL_MS)
  }

  /**
   * Decode a `cursor:shape` payload into an `ImageBitmap` and stash
   * it in the cursor shape cache. Fire-and-forget: a failed decode
   * leaves the cache unchanged so the paint loop keeps drawing the
   * previous shape (visually: a brief cursor freeze, not a crash).
   */
  async function applyCursorShape(
    msg: Record<string, unknown>,
  ): Promise<void> {
    const id = Number(msg.id)
    const w = Number(msg.w)
    const h = Number(msg.h)
    const hx = Number(msg.hx)
    const hy = Number(msg.hy)
    const b64 = msg.bgra
    if (!Number.isFinite(id) || !Number.isFinite(w) || !Number.isFinite(h) || typeof b64 !== 'string') {
      return
    }
    // Skip if we already have this shape cached — agent should only
    // send it on change but defensive.
    if (cursor.value.shapes.has(id)) return
    try {
      const bgra = base64ToBytes(b64)
      if (bgra.length < w * h * 4) return
      // Swizzle BGRA → RGBA for ImageData. Done in-place on a copy
      // so the original buffer is reusable.
      const rgba = new Uint8ClampedArray(w * h * 4)
      for (let i = 0; i < w * h; i++) {
        rgba[i * 4 + 0] = bgra[i * 4 + 2]! // R
        rgba[i * 4 + 1] = bgra[i * 4 + 1]! // G
        rgba[i * 4 + 2] = bgra[i * 4 + 0]! // B
        rgba[i * 4 + 3] = bgra[i * 4 + 3]! // A
      }
      const imgData = new ImageData(rgba, w, h)
      const bitmap = await createImageBitmap(imgData)
      // Mutate the Map in place + replace the ref to trigger Vue
      // reactivity (shallowRef would be nicer; ref + new object
      // reference works today).
      const shapes = new Map(cursor.value.shapes)
      shapes.set(id, { bitmap, hotspotX: hx, hotspotY: hy })
      cursor.value = { ...cursor.value, shapes }
    } catch {
      /* decode failed — skip this shape update */
    }
  }

  function stopStatsPoll() {
    if (statsTimer !== null) {
      clearInterval(statsTimer)
      statsTimer = null
    }
    statsPrevBytes = 0
    statsPrevTsMs = 0
    stats.value = { ...EMPTY_STATS }
  }

  /** Buffer ICE candidates that arrive before we've set a remote
   *  description, otherwise addIceCandidate throws. */
  const pendingRemoteIce: RTCIceCandidateInit[] = []
  let remoteDescriptionSet = false

  function installRcHandlers() {
    ws.onRcMessage('rc:session.created', (msg) => {
      sessionId.value = msg.session_id
      phase.value = 'awaiting_consent'
    })
    ws.onRcMessage('rc:ready', async (msg) => {
      if (!pc) return
      phase.value = 'negotiating'
      try {
        const offer = await pc.createOffer()
        await pc.setLocalDescription(offer)
        ws.sendRaw({
          t: 'rc:sdp.offer',
          session_id: msg.session_id,
          sdp: offer.sdp,
        })
      } catch (e) {
        failWith((e as Error).message || 'createOffer failed')
      }
    })
    ws.onRcMessage('rc:sdp.answer', async (msg) => {
      if (!pc) return
      try {
        await pc.setRemoteDescription({ type: 'answer', sdp: msg.sdp })
        remoteDescriptionSet = true
        // Flush any ICE that arrived early.
        for (const c of pendingRemoteIce) {
          try {
            await pc.addIceCandidate(c)
          } catch {
            /* tolerate stale candidates */
          }
        }
        pendingRemoteIce.length = 0
      } catch (e) {
        failWith((e as Error).message || 'setRemoteDescription failed')
      }
    })
    ws.onRcMessage('rc:ice', async (msg) => {
      if (!pc || !msg.candidate) return
      const init = msg.candidate as RTCIceCandidateInit
      if (!remoteDescriptionSet) {
        pendingRemoteIce.push(init)
        return
      }
      try {
        await pc.addIceCandidate(init)
      } catch {
        /* ignore — happens on stale candidates during teardown */
      }
    })
    ws.onRcMessage('rc:terminate', (msg) => {
      phase.value = 'closed'
      if (msg.reason) {
        // Reason is informational; UI surfaces it when non-nominal.
        if (msg.reason === 'error' || msg.reason === 'consent_timeout' || msg.reason === 'user_denied') {
          error.value = msg.reason
        }
      }
      teardown()
    })
    ws.onRcMessage('rc:error', (msg) => {
      failWith(msg.message || msg.code || 'signalling error')
    })
  }

  function removeRcHandlers() {
    ws.offRcMessage('rc:session.created')
    ws.offRcMessage('rc:ready')
    ws.offRcMessage('rc:sdp.answer')
    ws.offRcMessage('rc:ice')
    ws.offRcMessage('rc:terminate')
    ws.offRcMessage('rc:error')
  }

  function failWith(message: string) {
    error.value = message
    phase.value = 'error'
    teardown()
  }

  function teardown() {
    stopStatsPoll()
    stopWebCodecsPath()
    for (const ch of Object.values(channels)) {
      try { ch.close() } catch { /* ignore */ }
    }
    for (const k of Object.keys(channels)) delete channels[k]
    if (pc) {
      try { pc.close() } catch { /* ignore */ }
      pc = null
    }
    remoteStream.value = null
    hasMedia.value = false
    remoteDescriptionSet = false
    pendingRemoteIce.length = 0
    cursor.value = { pos: null, shapes: new Map() }
    mediaIntrinsicW.value = 0
    mediaIntrinsicH.value = 0
  }

  async function connect(agentId: string, permissions = 'VIEW | INPUT | CLIPBOARD') {
    if (phase.value !== 'idle' && phase.value !== 'closed' && phase.value !== 'error') {
      return // already active
    }
    error.value = null
    sessionId.value = null
    phase.value = 'requesting'

    // Restore the per-agent resolution preference. This has to live
    // here (not at composable-init) because `useRemoteControl()` runs
    // before the route params resolve on some mount paths, and we
    // don't want a stale value from a different agent leaking in.
    resolutionAgentId = agentId
    resolution.value = readStoredResolution(agentId)

    // Inspect what video codecs this browser can decode so the agent
    // can pick the best intersection with its own AgentCaps.codecs
    // (Phase 2 negotiation, 2B.2). Filtered to the codecs we'd ever
    // negotiate: H.264 universal, H.265 + AV1 = bandwidth wins,
    // VP9 = WebRTC-mandatory, VP8 = legacy. Browsers without
    // RTCRtpReceiver.getCapabilities (older Safari/Firefox) get an
    // empty list and the agent falls back to H.264-only.
    const allBrowserCaps = inspectBrowserVideoCodecs()
    const browserCaps = filterCapsByPreference(allBrowserCaps, preferredCodec.value)
    if (allBrowserCaps.length > 0) {
      // Surface both lists in the console — useful when debugging
      // "why didn't H.265 negotiate" on a session. Shown as the raw
      // browser list ∩ forced preference → sent list.
      console.info(
        '[rc] browser codecs:',
        allBrowserCaps.join(', '),
        preferredCodec.value ? `(forced ${preferredCodec.value})` : '',
        '→ sending to agent:',
        browserCaps.join(', '),
      )
    }

    // Pull TURN creds before creating the PC so the first gather uses them.
    let iceServers: IceServer[] = []
    try {
      const creds = await api.get<TurnCredsResponse>('/turn/credentials')
      iceServers = creds.ice_servers
    } catch {
      // Fall back to a public STUN if the server has none configured.
      iceServers = [{ urls: ['stun:stun.l.google.com:19302'] }]
    }

    pc = new RTCPeerConnection({
      iceServers: iceServers as RTCIceServer[],
      bundlePolicy: 'max-bundle',
    })

    pc.ontrack = (ev) => {
      // Replace rather than append. addTrack accumulates across ICE
      // restarts / renegotiations, leaving dead tracks attached to the
      // MediaStream; the <video> element would render the wrong one.
      // Current agent doesn't renegotiate, but if it ever does this
      // would regress silently — replacement is idempotent for the
      // single-track case we have today.
      remoteStream.value = new MediaStream([ev.track])
      hasMedia.value = true
      // Try the WebCodecs bypass first when the user opted in AND the
      // browser supports it. If the canvas hasn't mounted yet (common
      // — ontrack fires in 'negotiating' while the canvas is gated on
      // 'connected'), stash the receiver; the watcher on
      // webcodecsCanvasEl below picks it up as soon as the canvas
      // mounts.
      const wantsWebCodecs =
        renderPath.value === 'webcodecs'
        && webcodecsSupported.value
        && ev.track.kind === 'video'
      if (wantsWebCodecs) {
        // Hint the default receiver path toward low-latency so the
        // brief window before the transform lands doesn't buffer.
        try {
          const receiver = ev.receiver as RTCRtpReceiver & {
            jitterBufferTarget?: number | null
            playoutDelayHint?: number | null
          }
          receiver.jitterBufferTarget = 0
          receiver.playoutDelayHint = 0
        } catch { /* best-effort */ }
        // Install the transform EAGERLY — canvas is attached later
        // when it mounts. This gets Chrome's RTP pipeline routing
        // frames to our worker from the first packet; waiting for
        // the canvas mount (phase === 'connected') meant the default
        // decoder locked in first on some Chrome builds and the
        // transform stopped receiving anything.
        if (installWebCodecsTransform(ev.receiver)) {
          return
        }
        // Install failed (no RTCRtpScriptTransform, worker throw,
        // etc.) — fall through to classic <video> path.
      }
      // Tell the browser we care about latency, not playback smoothness.
      // Chromium enforces a soft ~80 ms floor regardless, but asking
      // for zero still shaves ~30-50 ms off the previous 50 ms setting
      // because the jitter-buffer overhead is both the floor AND the
      // requested target. See
      // https://www.w3.org/TR/webrtc-extensions/#dom-rtcrtpreceiver-jitterbuffertarget
      try {
        const receiver = ev.receiver as RTCRtpReceiver & {
          jitterBufferTarget?: number | null
          playoutDelayHint?: number | null
        }
        receiver.jitterBufferTarget = 0
        // Firefox + non-standard Chromium hint — belt-and-braces with
        // jitterBufferTarget. Same intent: "decode + display as fast
        // as possible; I'd rather see stutter than lag."
        receiver.playoutDelayHint = 0
      } catch {
        // Best-effort — browser will use its own adaptive default.
      }
      // contentHint tells the compositor this is motion (not detail),
      // which switches Chrome's <video> internal smoothing off and
      // discourages re-buffering on minor frame timing irregularity.
      try {
        (ev.track as MediaStreamTrack & { contentHint?: string }).contentHint = 'motion'
      } catch {
        /* ignore */
      }
    }

    pc.onicecandidate = (ev) => {
      if (!sessionId.value) return
      // Note: null candidate signals end-of-gather — skip it.
      if (!ev.candidate) return
      ws.sendRaw({
        t: 'rc:ice',
        session_id: sessionId.value,
        candidate: ev.candidate.toJSON(),
      })
    }

    pc.onconnectionstatechange = () => {
      // Snapshot the state up front: failWith() below nulls `pc` as part
      // of teardown, so re-reading `pc.connectionState` on the next branch
      // would throw TypeError.
      const state = pc?.connectionState
      if (!state) return
      if (state === 'connected') phase.value = 'connected'
      else if (state === 'failed') failWith('peer connection failed')
      else if (state === 'closed' && phase.value !== 'error' && phase.value !== 'closed') {
        // Clean up the data channels + stream too; otherwise they leak
        // when the PC closes without a prior disconnect() (e.g. the
        // server-side session terminates first).
        phase.value = 'closed'
        teardown()
      }
    }

    // Declare we want to *receive* video from the agent. Without this line
    // the offer has no m=video section, so the agent's answer can't include
    // one either — ontrack never fires and hasMedia stays false. See the
    // peer-side mirror in agents/roomler-agent/src/peer.rs (add_track).
    pc.addTransceiver('video', { direction: 'recvonly' })

    // Create the four data channels up front per architecture doc §5.
    // Reliability profiles match the doc: unreliable+unordered for input,
    // reliable+ordered for everything else.
    channels.input = pc.createDataChannel('input', {
      ordered: false,
      maxRetransmits: 0,
    })
    channels.control = pc.createDataChannel('control', { ordered: true })
    // Cursor channel: reliable + ordered because a dropped `cursor:
    // shape` message would leave the browser unable to render the
    // current cursor. Position-only updates would also be fine
    // unordered, but muxing both on one channel means we use the
    // stricter policy.
    channels.cursor = pc.createDataChannel('cursor', { ordered: true })
    channels.clipboard = pc.createDataChannel('clipboard', { ordered: true })
    channels.files = pc.createDataChannel('files', { ordered: true })

    // Subscribe to the clipboard DC. Agent -> browser messages are
    // `clipboard:content` (reply to a read) and `clipboard:error`
    // (read or write failure). Pending-read promises are keyed by the
    // req_id we stamp on outbound `clipboard:read` messages so
    // interleaved reads resolve independently.
    channels.clipboard.onmessage = (ev) => {
      if (typeof ev.data !== 'string') return
      let msg: { t?: string; req_id?: number | null; text?: string; message?: string }
      try {
        msg = JSON.parse(ev.data)
      } catch {
        return
      }
      if (msg.t === 'clipboard:content') {
        const reqId = typeof msg.req_id === 'number' ? msg.req_id : null
        if (reqId == null) return
        const pending = pendingClipboardReads.get(reqId)
        if (!pending) return
        clearTimeout(pending.timer)
        pendingClipboardReads.delete(reqId)
        pending.resolve(typeof msg.text === 'string' ? msg.text : '')
      } else if (msg.t === 'clipboard:error') {
        const reqId = typeof msg.req_id === 'number' ? msg.req_id : null
        if (reqId == null) return
        const pending = pendingClipboardReads.get(reqId)
        if (!pending) return
        clearTimeout(pending.timer)
        pendingClipboardReads.delete(reqId)
        pending.reject(new Error(msg.message || 'agent clipboard error'))
      }
    }

    // Subscribe to the cursor DC. The agent pumps `cursor:pos` /
    // `cursor:shape` / `cursor:hide` at ~30 Hz; decode shape bitmaps
    // eagerly so the paint loop is a zero-copy `drawImage`.
    channels.cursor.onmessage = (ev) => {
      if (typeof ev.data !== 'string') return
      let msg: { t?: string } & Record<string, unknown>
      try {
        msg = JSON.parse(ev.data)
      } catch {
        return
      }
      if (msg.t === 'cursor:pos') {
        const id = Number(msg.id)
        const x = Number(msg.x)
        const y = Number(msg.y)
        if (Number.isFinite(id) && Number.isFinite(x) && Number.isFinite(y)) {
          cursor.value = { ...cursor.value, pos: { id, x, y } }
        }
      } else if (msg.t === 'cursor:shape') {
        void applyCursorShape(msg)
      } else if (msg.t === 'cursor:hide') {
        cursor.value = { ...cursor.value, pos: null }
      }
    }

    installRcHandlers()

    // Flag the first open so the input pump can start queuing.
    channels.input.onopen = () => { inputChannelOpen.value = true }
    channels.input.onclose = () => { inputChannelOpen.value = false }

    // Re-send the restored quality preference as soon as the control
    // channel opens — otherwise the agent would stay at its default
    // after a page reload that had set a non-default preference.
    channels.control.onopen = () => {
      sendQualityPreference()
      sendResolutionPreference()
    }

    // Begin polling getStats() on a 500 ms cadence so the UI can show
    // live bitrate/fps/codec. Runs unconditionally while `pc` exists;
    // teardown() stops + clears it.
    startStatsPoll()

    // Kick off the rc:* handshake. browser_caps lets the agent pick
    // the best codec on its end (Phase 2 commit 2B.2 wires the
    // intersection logic + SDP munging on the agent side).
    ws.sendRaw({
      t: 'rc:session.request',
      agent_id: agentId,
      permissions,
      browser_caps: browserCaps,
    })
  }

  function disconnect() {
    if (sessionId.value && pc) {
      ws.sendRaw({
        t: 'rc:terminate',
        session_id: sessionId.value,
        reason: 'controller_hangup',
      })
    }
    phase.value = 'closed'
    teardown()
    removeRcHandlers()
  }

  onBeforeUnmount(() => {
    disconnect()
  })

  /**
   * Attach mouse/keyboard/wheel listeners to a surface element (typically
   * the video container). Coordinates sent to the agent are normalised in
   * `[0,1]` per the architecture doc §6, so the agent can resolve them
   * against its current resolution.
   *
   * Returns a detach function the caller should invoke before unmounting.
   */
  function attachInput(surface: HTMLElement): () => void {
    // Locate the <video> once, fall back gracefully if the layout changes.
    const findVideo = () =>
      (surface.querySelector('video') as HTMLVideoElement | null) ??
      (surface.firstElementChild as HTMLVideoElement | null)
    const clamp01 = (n: number) => Math.min(Math.max(n, 0), 1)

    /**
     * Returns [0,1]-normalised coordinates relative to the *visible video
     * content* — not the outer .video-frame. The `<video>` uses
     * `object-fit: contain`, which letterboxes the stream when the display
     * aspect ratio differs from the source (e.g. 2560x1600 viewport showing
     * a 3840x2160 agent). Without this correction, clicks land at the wrong
     * pixel on the remote, and clicks in the letterbox bars get clamped to
     * the edge instead of being ignored.
     */
    function normalisedXY(
      ev: PointerEvent | MouseEvent | WheelEvent,
    ): { x: number; y: number; insideVideo: boolean } {
      const video = findVideo()
      // In `original` / `custom` scale modes the `<video>` element is
      // sized to its own intrinsic pixels (× custom scale) — there's
      // no letterboxing inside it, so map directly against its
      // bounding rect. In `adaptive` mode the element fills the stage
      // and `object-fit: contain` letterboxes internally, so we need
      // the stage rect + aspect-ratio math.
      if (scaleMode.value !== 'adaptive' && video) {
        const videoRect = video.getBoundingClientRect()
        return directVideoNormalise(
          ev.clientX, ev.clientY,
          { left: videoRect.left, top: videoRect.top, width: videoRect.width, height: videoRect.height },
        )
      }
      const frameRect = surface.getBoundingClientRect()
      return letterboxedNormalise(
        ev.clientX, ev.clientY,
        { left: frameRect.left, top: frameRect.top, width: frameRect.width, height: frameRect.height },
        video?.videoWidth ?? 0, video?.videoHeight ?? 0,
      )
    }

    function onPointerMove(ev: PointerEvent) {
      const { x, y, insideVideo } = normalisedXY(ev)
      if (!insideVideo) return
      pendingMove = { x, y, mon: 0 }
      if (rafHandle === null) rafHandle = requestAnimationFrame(flushPendingMove)
    }

    // Cancel any RAF-queued mouse_move so it can't fire *after* a click
    // and overwrite whatever move the user does next. Without this, a
    // fast click-then-drag can register a stale mouse_move at the click
    // coords between the button event and the subsequent moves.
    function cancelPendingMove() {
      if (rafHandle !== null) {
        cancelAnimationFrame(rafHandle)
        rafHandle = null
      }
      pendingMove = null
    }

    function onPointerDown(ev: PointerEvent) {
      ev.preventDefault()
      const { x, y, insideVideo } = normalisedXY(ev)
      if (!insideVideo) return
      cancelPendingMove()
      surface.setPointerCapture(ev.pointerId)
      sendInput({ t: 'mouse_button', btn: browserButton(ev.button), down: true, x, y, mon: 0 })
    }

    function onPointerUp(ev: PointerEvent) {
      try { surface.releasePointerCapture(ev.pointerId) } catch { /* noop */ }
      const { x, y, insideVideo } = normalisedXY(ev)
      if (!insideVideo) return
      cancelPendingMove()
      sendInput({ t: 'mouse_button', btn: browserButton(ev.button), down: false, x, y, mon: 0 })
    }

    function onWheel(ev: WheelEvent) {
      ev.preventDefault()
      // Browser uses positive Y for down; agent does the same.
      sendInput({
        t: 'mouse_wheel',
        dx: ev.deltaX,
        dy: ev.deltaY,
        mode: ev.deltaMode === 0 ? 'pixel' : ev.deltaMode === 1 ? 'line' : 'page',
      })
    }

    // Track whether the pointer is currently over the remote-viewer
    // surface. When true, browser-eaten shortcuts like Ctrl+A / Ctrl+C
    // are intercepted locally (preventDefault) and forwarded to the
    // remote only. When false, the controller keeps normal browser UX
    // (Ctrl+T opens a new tab, Ctrl+F triggers find, etc.).
    let pointerInside = false
    function onPointerEnter() { pointerInside = true }
    function onPointerLeave() { pointerInside = false }

    function onKey(ev: KeyboardEvent, down: boolean) {
      const code = kbdCodeToHid(ev.code)
      if (code === null) return
      const mods =
        (ev.ctrlKey ? 1 : 0) | (ev.shiftKey ? 2 : 0) | (ev.altKey ? 4 : 0) | (ev.metaKey ? 8 : 0)
      if (shouldPreventDefault(ev, pointerInside)) ev.preventDefault()
      sendInput({ t: 'key', code, down, mods })
    }

    const onKeyDown = (e: KeyboardEvent) => onKey(e, true)
    const onKeyUp = (e: KeyboardEvent) => onKey(e, false)

    // Disable the OS-native context menu so right-click forwards cleanly.
    function onContextMenu(ev: MouseEvent) { ev.preventDefault() }

    surface.addEventListener('pointermove', onPointerMove)
    surface.addEventListener('pointerdown', onPointerDown)
    surface.addEventListener('pointerup', onPointerUp)
    surface.addEventListener('pointerenter', onPointerEnter)
    surface.addEventListener('pointerleave', onPointerLeave)
    surface.addEventListener('wheel', onWheel, { passive: false })
    surface.addEventListener('contextmenu', onContextMenu)
    // Keys are captured on window so they fire even if the video loses focus.
    window.addEventListener('keydown', onKeyDown)
    window.addEventListener('keyup', onKeyUp)

    return () => {
      surface.removeEventListener('pointermove', onPointerMove)
      surface.removeEventListener('pointerdown', onPointerDown)
      surface.removeEventListener('pointerup', onPointerUp)
      surface.removeEventListener('pointerenter', onPointerEnter)
      surface.removeEventListener('pointerleave', onPointerLeave)
      surface.removeEventListener('wheel', onWheel)
      surface.removeEventListener('contextmenu', onContextMenu)
      window.removeEventListener('keydown', onKeyDown)
      window.removeEventListener('keyup', onKeyUp)
      if (rafHandle !== null) cancelAnimationFrame(rafHandle)
    }
  }

  /** Send the browser's clipboard text to the agent's OS clipboard.
   *  Fire-and-forget. Requires user gesture — `navigator.clipboard.
   *  readText()` throws in non-gesture contexts. Call from a button
   *  click handler. Resolves to `true` on best-effort send, `false`
   *  if the clipboard DC isn't open or reading the browser clipboard
   *  was blocked (e.g. permissions denied). */
  async function sendClipboardToAgent(): Promise<boolean> {
    const ch = channels.clipboard
    if (!ch || ch.readyState !== 'open') return false
    let text: string
    try {
      text = await globalThis.navigator.clipboard.readText()
    } catch {
      return false
    }
    try {
      ch.send(JSON.stringify({ t: 'clipboard:write', text }))
      return true
    } catch {
      return false
    }
  }

  /** Request the agent's current clipboard text. Rejects with a
   *  timeout after 5 seconds if the agent doesn't reply. Call from a
   *  button click handler so the subsequent `navigator.clipboard.
   *  writeText()` has user-gesture permission. Resolves with the
   *  text; the caller is responsible for writing it to the browser
   *  clipboard (this lets the caller show a preview / paste into a
   *  specific field instead of always overwriting). */
  function getAgentClipboard(): Promise<string> {
    const ch = channels.clipboard
    if (!ch || ch.readyState !== 'open') {
      return Promise.reject(new Error('clipboard channel not open'))
    }
    const reqId = nextClipboardReqId++
    const msg = JSON.stringify({ t: 'clipboard:read', req_id: reqId })
    return new Promise<string>((resolve, reject) => {
      const timer = setTimeout(() => {
        pendingClipboardReads.delete(reqId)
        reject(new Error('agent did not respond to clipboard:read within 5s'))
      }, 5000)
      pendingClipboardReads.set(reqId, { resolve, reject, timer })
      try {
        ch.send(msg)
      } catch (e) {
        clearTimeout(timer)
        pendingClipboardReads.delete(reqId)
        reject(e instanceof Error ? e : new Error(String(e)))
      }
    })
  }

  /** Upload a `File` to the remote host's Downloads folder via the
   *  `files` data channel. Chunks at 64 KiB, yielding between sends so
   *  the DC's sctp send buffer doesn't blow out on large files.
   *  Resolves with the final path + byte count reported by the agent.
   *  Rejects on agent error or if the channel isn't open. */
  function uploadFile(file: File): Promise<{ path: string; bytes: number }> {
    const ch = channels.files
    if (!ch || ch.readyState !== 'open') {
      return Promise.reject(new Error('files channel not open'))
    }
    const id = `up-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`

    return new Promise((resolve, reject) => {
      // Per-upload listener: the `files` DC is multiplexed in theory
      // but today we serialize, so matching on id is enough.
      const onMessage = (ev: MessageEvent) => {
        if (typeof ev.data !== 'string') return
        let msg: { t?: string; id?: string; path?: string; bytes?: number; message?: string }
        try { msg = JSON.parse(ev.data) } catch { return }
        if (msg.id !== id) return
        if (msg.t === 'files:complete') {
          ch.removeEventListener('message', onMessage)
          resolve({ path: String(msg.path ?? ''), bytes: Number(msg.bytes ?? 0) })
        } else if (msg.t === 'files:error') {
          ch.removeEventListener('message', onMessage)
          reject(new Error(msg.message || 'agent rejected upload'))
        }
        // files:accepted / files:progress — pure observations, no-op.
      }
      ch.addEventListener('message', onMessage)

      try {
        ch.send(JSON.stringify({
          t: 'files:begin',
          id,
          name: file.name,
          size: file.size,
          mime: file.type || undefined,
        }))
      } catch (e) {
        ch.removeEventListener('message', onMessage)
        reject(e instanceof Error ? e : new Error(String(e)))
        return
      }

      const CHUNK = 64 * 1024
      let offset = 0
      const pump = async () => {
        try {
          while (offset < file.size) {
            // Back off when the sctp buffer starts filling up so the
            // browser doesn't OOM on huge files.
            while (ch.bufferedAmount > 4 * 1024 * 1024) {
              await new Promise((r) => setTimeout(r, 20))
            }
            const end = Math.min(offset + CHUNK, file.size)
            const slice = file.slice(offset, end)
            const buf = await slice.arrayBuffer()
            ch.send(buf)
            offset = end
          }
          ch.send(JSON.stringify({ t: 'files:end', id }))
        } catch (e) {
          ch.removeEventListener('message', onMessage)
          reject(e instanceof Error ? e : new Error(String(e)))
        }
      }
      void pump()
    })
  }

  /** Send Ctrl+Alt+Del to the remote. The browser can't capture this
   *  key combo (the OS intercepts first), so callers typically wire
   *  this to a dedicated toolbar button. Emits the three down events
   *  in the canonical order (Ctrl→Alt→Del) followed by releases in
   *  reverse, matching the native SAS ordering. */
  function sendCtrlAltDel() {
    const ch = channels.input
    if (!ch || ch.readyState !== 'open') return
    // HID usage codes: LeftCtrl=0xe0, LeftAlt=0xe2, Delete=0x4c
    // mods bitfield: ctrl=1, shift=2, alt=4, meta=8
    const send = (msg: Record<string, unknown>) => {
      try { ch.send(JSON.stringify(msg)) } catch { /* dropped */ }
    }
    send({ t: 'key', code: 0xe0, down: true, mods: 1 })
    send({ t: 'key', code: 0xe2, down: true, mods: 1 | 4 })
    send({ t: 'key', code: 0x4c, down: true, mods: 1 | 4 })
    send({ t: 'key', code: 0x4c, down: false, mods: 1 | 4 })
    send({ t: 'key', code: 0xe2, down: false, mods: 1 })
    send({ t: 'key', code: 0xe0, down: false, mods: 0 })
  }

  return {
    phase,
    error,
    sessionId,
    remoteStream,
    hasMedia,
    inputChannelOpen,
    stats,
    quality,
    setQuality,
    cursor,
    connect,
    disconnect,
    attachInput,
    sendClipboardToAgent,
    getAgentClipboard,
    sendCtrlAltDel,
    uploadFile,
    preferredCodec,
    setPreferredCodec,
    scaleMode,
    scaleCustomPercent,
    setScaleMode,
    setScaleCustomPercent,
    resolution,
    setResolution,
    renderPath,
    setRenderPath,
    webcodecsSupported,
    webcodecsCanvasEl,
    mediaIntrinsicW,
    mediaIntrinsicH,
  }
}

/** Small base64 → Uint8Array helper. atob + TextDecoder would work
 *  but base64 decodes to a binary string that atob(str).charCodeAt(i)
 *  handles correctly; use the direct loop. Exported for tests. */
export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

/**
 * Inspect the browser's `RTCRtpReceiver.getCapabilities('video')` and
 * return the subset of codec mime types we care about for negotiation,
 * stripped to short names ("h264", "h265", "av1", "vp9", "vp8").
 *
 * Returns an empty array on browsers that don't expose
 * `getCapabilities` (older Safari/iOS) — the agent then falls back to
 * H.264-only. Each codec is reported once even if the browser
 * advertises multiple profile-level-id variants of it.
 *
 * Exported standalone so vitest can verify the filter without standing
 * up the full composable + WS store.
 */
export function inspectBrowserVideoCodecs(): string[] {
  // Static method. Older Safari may not have it.
  const getCaps = (
    globalThis as unknown as { RTCRtpReceiver?: { getCapabilities?: (k: string) => { codecs?: Array<{ mimeType?: string }> } | null } }
  ).RTCRtpReceiver?.getCapabilities
  if (typeof getCaps !== 'function') return []
  const caps = getCaps('video')
  if (!caps || !Array.isArray(caps.codecs)) return []
  // Codec mime types are case-insensitive per RFC 6381 ("video/H264").
  const seen = new Set<string>()
  for (const c of caps.codecs) {
    const mime = (c.mimeType || '').toLowerCase()
    if (!mime.startsWith('video/')) continue
    const name = mime.slice('video/'.length)
    // Filter to the codecs the agent's negotiation cares about.
    // RTX (retransmission), red (FEC), ulpfec, flexfec are RTP
    // mechanism codecs — not what we'd negotiate as the primary
    // video codec.
    if (['h264', 'h265', 'av1', 'vp9', 'vp8'].includes(name)) {
      seen.add(name)
    }
  }
  return Array.from(seen)
}

/**
 * Pure helper: extract a live-stats snapshot from an `RTCStatsReport`.
 *
 * Given the previous `bytesReceived` total and its wall-clock timestamp,
 * computes the delta bitrate over the interval and returns it along with
 * the new cumulative counters so the caller can feed them back on the
 * next poll. Extracted as a pure function so the bitrate/fps/codec
 * derivation can be unit-tested without a real PeerConnection.
 *
 * Bitrate is 0 on the first call (prevTsMs === 0): we need two
 * snapshots to derive a rate. `codec` comes from matching the
 * `inbound-rtp.codecId` against a `codec.id` in the same report.
 */
export function extractStatsSnapshot(
  report: RTCStatsReport,
  prevBytes: number,
  prevTsMs: number,
): { next: RcStats; bytes: number; tsMs: number } {
  let bytes = 0
  let tsMs = 0
  let fps = 0
  let codecId = ''
  const codecMap = new Map<string, string>()

  report.forEach((raw) => {
    // RTCStatsReport is typed loosely — narrow via `type`.
    const s = raw as { type?: string } & Record<string, unknown>
    if (s.type === 'inbound-rtp' && (s as { kind?: string }).kind === 'video') {
      bytes = typeof s.bytesReceived === 'number' ? s.bytesReceived : 0
      tsMs = typeof s.timestamp === 'number' ? s.timestamp : 0
      fps = typeof s.framesPerSecond === 'number' ? s.framesPerSecond : 0
      codecId = typeof s.codecId === 'string' ? s.codecId : ''
    } else if (s.type === 'codec') {
      const id = typeof s.id === 'string' ? s.id : ''
      const mime = typeof s.mimeType === 'string' ? s.mimeType : ''
      if (id) codecMap.set(id, mime)
    }
  })

  // mimeType shape: "video/H264" → strip the prefix for display.
  const mime = codecMap.get(codecId) || ''
  const codec = mime.replace(/^video\//i, '')

  let bitrate_bps = 0
  if (prevTsMs > 0 && tsMs > prevTsMs) {
    const dtSec = (tsMs - prevTsMs) / 1000
    bitrate_bps = Math.max(0, Math.round(((bytes - prevBytes) * 8) / dtSec))
  }

  return {
    next: { bitrate_bps, fps: Math.round(fps * 10) / 10, codec },
    bytes,
    tsMs,
  }
}

/**
 * Map a browser `MouseEvent.button` (0/1/2/3/4) to the agent's enum.
 */
function browserButton(n: number): 'left' | 'right' | 'middle' | 'back' | 'forward' {
  switch (n) {
    case 0: return 'left'
    case 1: return 'middle'
    case 2: return 'right'
    case 3: return 'back'
    case 4: return 'forward'
    default: return 'left'
  }
}

/** Decide whether to `preventDefault` on a keyboard event in the remote
 *  viewer. Two categories:
 *
 *  1. Unconditionally: `Tab` (would otherwise move focus out of the
 *     video and away from our key listeners) and plain `Backspace`
 *     (some browsers map to back-navigation on pages without a form).
 *
 *  2. Only when the pointer is over the video: common Ctrl/Cmd-shortcuts
 *     that the local browser would otherwise intercept (Ctrl+A select
 *     all, Ctrl+C/V/X clipboard, Ctrl+Z/Y undo/redo, Ctrl+F find,
 *     Ctrl+S save, Ctrl+P print, Ctrl+R reload). Outside the video
 *     the controller keeps normal browser UX — Ctrl+T to open a tab,
 *     Ctrl+W to close it, etc.
 *
 *  Ctrl+Alt+Del is reserved by the OS and cannot be intercepted by the
 *  browser — it's exposed via the dedicated toolbar button instead.
 *  Exported so unit tests can lock the policy. */
export function shouldPreventDefault(ev: KeyboardEvent, pointerInside: boolean): boolean {
  if (ev.code === 'Tab') return true
  if (ev.code === 'Backspace' && !ev.ctrlKey && !ev.altKey && !ev.metaKey) return true
  if (!pointerInside) return false
  const cmd = ev.ctrlKey || ev.metaKey
  if (!cmd) return false
  // Keys the local browser would intercept; prevent so they only
  // forward to the remote.
  switch (ev.code) {
    case 'KeyA': case 'KeyC': case 'KeyV': case 'KeyX':
    case 'KeyZ': case 'KeyY':
    case 'KeyF': case 'KeyS': case 'KeyP': case 'KeyR':
      return true
    default:
      return false
  }
}

/**
 * Translate `KeyboardEvent.code` (physical-key string, e.g. "KeyA",
 * "ArrowLeft") to a USB HID usage code on the Keyboard/Keypad page.
 *
 * The agent's enigo backend maps these back to OS-native scan codes,
 * which is what makes remote typing layout-independent.
 */
function kbdCodeToHid(code: string): number | null {
  // Letter row.
  if (code.startsWith('Key') && code.length === 4) {
    const ch = code.charCodeAt(3) - 'A'.charCodeAt(0)
    if (ch >= 0 && ch <= 25) return 0x04 + ch // a..z → 0x04..0x1d
  }
  // Digit row.
  if (code.startsWith('Digit') && code.length === 6) {
    const d = code.charCodeAt(5) - '0'.charCodeAt(0)
    // HID: 1..9 → 0x1e..0x26, 0 → 0x27
    if (d === 0) return 0x27
    if (d >= 1 && d <= 9) return 0x1e + d - 1
  }
  if (code === 'Enter') return 0x28
  if (code === 'Escape') return 0x29
  if (code === 'Backspace') return 0x2a
  if (code === 'Tab') return 0x2b
  if (code === 'Space') return 0x2c
  if (code === 'ArrowRight') return 0x4f
  if (code === 'ArrowLeft') return 0x50
  if (code === 'ArrowDown') return 0x51
  if (code === 'ArrowUp') return 0x52
  if (code === 'Home') return 0x4a
  if (code === 'End') return 0x4d
  if (code === 'PageUp') return 0x4b
  if (code === 'PageDown') return 0x4e
  if (code === 'Insert') return 0x49
  if (code === 'Delete') return 0x4c
  if (code === 'ControlLeft') return 0xe0
  if (code === 'ShiftLeft') return 0xe1
  if (code === 'AltLeft') return 0xe2
  if (code === 'MetaLeft') return 0xe3
  if (code === 'ControlRight') return 0xe4
  if (code === 'ShiftRight') return 0xe5
  if (code === 'AltRight') return 0xe6
  if (code === 'MetaRight') return 0xe7
  // F1..F12
  if (code.startsWith('F') && code.length >= 2 && code.length <= 3) {
    const n = parseInt(code.slice(1), 10)
    if (n >= 1 && n <= 12) return 0x3a + n - 1
  }
  return null
}

/**
 * Pure helper: given a pointer clientX/clientY, the .video-frame bounding
 * rect, and the `<video>` element's intrinsic videoWidth/videoHeight,
 * return [0,1]-normalised coordinates relative to the *visible video
 * content* (accounting for the letterbox that `object-fit: contain`
 * produces when viewer and agent aspect ratios differ) plus a boolean
 * indicating whether the pointer is inside the visible region.
 *
 * Extracted so the math is unit-testable without a DOM.
 */
export function letterboxedNormalise(
  clientX: number,
  clientY: number,
  frame: { left: number; top: number; width: number; height: number },
  videoWidth: number,
  videoHeight: number,
): { x: number; y: number; insideVideo: boolean } {
  const clamp01 = (n: number) => Math.min(Math.max(n, 0), 1)

  if (!videoWidth || !videoHeight || !frame.width || !frame.height) {
    // No aspect ratio yet — fall back to frame-relative coords.
    const x = (clientX - frame.left) / Math.max(frame.width, 1)
    const y = (clientY - frame.top) / Math.max(frame.height, 1)
    return { x: clamp01(x), y: clamp01(y), insideVideo: true }
  }

  const videoAR = videoWidth / videoHeight
  const frameAR = frame.width / frame.height
  let visibleW: number, visibleH: number, offsetX: number, offsetY: number
  if (videoAR > frameAR) {
    visibleW = frame.width
    visibleH = frame.width / videoAR
    offsetX = 0
    offsetY = (frame.height - visibleH) / 2
  } else {
    visibleW = frame.height * videoAR
    visibleH = frame.height
    offsetX = (frame.width - visibleW) / 2
    offsetY = 0
  }

  const localX = clientX - frame.left - offsetX
  const localY = clientY - frame.top - offsetY
  const insideVideo =
    localX >= 0 && localX <= visibleW && localY >= 0 && localY <= visibleH
  return {
    x: clamp01(localX / Math.max(visibleW, 1)),
    y: clamp01(localY / Math.max(visibleH, 1)),
    insideVideo,
  }
}

/**
 * Pure helper for `original` / `custom` scale modes where the `<video>`
 * element is rendered without letterboxing — at its intrinsic size or
 * with a uniform CSS scale. Coordinates map directly against the
 * video element's bounding rect (which already includes scroll
 * offset and the custom CSS scale), normalised to `[0,1]` relative to
 * that rect.
 *
 * Unlike `letterboxedNormalise` this doesn't need to know the
 * intrinsic `videoWidth/videoHeight` — the bounding rect already
 * reflects the rendered size after scroll + scale, so a point at
 * normalised `(0.5, 0.5)` is always the middle of the remote frame.
 */
export function directVideoNormalise(
  clientX: number,
  clientY: number,
  videoRect: { left: number; top: number; width: number; height: number },
): { x: number; y: number; insideVideo: boolean } {
  const clamp01 = (n: number) => Math.min(Math.max(n, 0), 1)
  if (!videoRect.width || !videoRect.height) {
    return { x: 0, y: 0, insideVideo: false }
  }
  const localX = clientX - videoRect.left
  const localY = clientY - videoRect.top
  const insideVideo =
    localX >= 0 && localX <= videoRect.width &&
    localY >= 0 && localY <= videoRect.height
  return {
    x: clamp01(localX / videoRect.width),
    y: clamp01(localY / videoRect.height),
    insideVideo,
  }
}

export { browserButton, kbdCodeToHid }
