import { ref, watch, onBeforeUnmount, computed, type Ref, type ComputedRef } from 'vue'
import { useWsStore } from '@/stores/ws'
import { api } from '@/api/client'
import type { Agent } from '@/stores/agents'

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
  | 'reconnecting'
  | 'closed'
  | 'error'

/**
 * Backoff ladder for the auto-reconnect path. The first three steps
 * (250 ms / 500 ms / 1 s) are tuned for desktop-transition recovery —
 * a Win+L lock or a M3 SYSTEM-context capture handoff usually
 * resolves in well under a second, and a 2 s first-retry would leave
 * a visible black-frame window every time the user touches the lock
 * screen. The last three (2 s / 4 s / 8 s) cover real network drops.
 * 6 attempts caps the worst case at ~16 s before we give up and
 * surface an error to the operator.
 */
export const RC_RECONNECT_LADDER_MS = [250, 500, 1000, 2000, 4000, 8000] as const

/**
 * Steady-state retry delay used AFTER `RC_RECONNECT_LADDER_MS` is
 * exhausted. rc.23 — the DC stays "open" from the operator's POV by
 * never surfacing a terminal "budget exhausted" state; the reconnect
 * loop keeps trying every `RC_RECONNECT_STEADY_MS` until the operator
 * cancels via the existing Cancel button or the upload completes via
 * resume. Field repro on PC50045 2026-05-11/12: large file uploads
 * fail with the legacy 6-attempt cap because ESET intercepts cause
 * repeated DC drops; an operator who walks away from their machine
 * came back to a failed upload with no diagnostic context. With the
 * cap removed, they can leave it retrying and inspect the log
 * panel (also new in rc.23) at their leisure.
 */
export const RC_RECONNECT_STEADY_MS = 8000

/**
 * Parse an inbound `control` data-channel message into a typed
 * value. Returns `null` for any non-JSON, non-string, or unknown
 * envelope shape so the caller can no-op silently. Recognised
 * variants:
 *   - `rc:host_locked` — boolean lock-state flip (agents 0.2.3+)
 *   - `rc:desktop_changed` — input desktop name (agents 0.3.0+
 *     SYSTEM-context worker; emitted on `try_change_desktop`
 *     Switched). Powers the secondary "On Winlogon" chip.
 * Future agent → browser control messages (rc:cursor-shape,
 * rc:dpi-change, ...) layer on the same parse-by-`t` switch.
 * Unknown `t` values fall through to `null` so older browsers
 * stay forward-compatible with newer agents.
 *
 * Exported for unit testing. Production code should consume the
 * already-applied `hostLocked` / `currentDesktop` refs from the
 * composable.
 */
export type RcLogsFetchReply = {
  ok: boolean
  /** Absolute path of the file the lines came from (when ok). */
  path?: string
  /** One string per line, in file order (oldest first). */
  lines?: string[]
  /** True when the file had more lines than were returned. */
  truncated?: boolean
  /** Error message when ok = false. */
  error?: string
}

export type RcControlInbound =
  | { kind: 'host_locked'; locked: boolean }
  | { kind: 'desktop_changed'; name: string }
  | { kind: 'logs_fetch_reply'; reply: RcLogsFetchReply }
  | null

export function parseControlInbound(data: unknown): RcControlInbound {
  if (typeof data !== 'string') return null
  let parsed: unknown
  try {
    parsed = JSON.parse(data)
  } catch {
    return null
  }
  if (parsed === null || typeof parsed !== 'object') return null
  const obj = parsed as Record<string, unknown>
  if (obj.t === 'rc:host_locked' && typeof obj.locked === 'boolean') {
    return { kind: 'host_locked', locked: obj.locked }
  }
  if (
    obj.t === 'rc:desktop_changed' &&
    typeof obj.name === 'string' &&
    obj.name.length > 0
  ) {
    return { kind: 'desktop_changed', name: obj.name }
  }
  if (obj.t === 'rc:logs-fetch.reply') {
    const reply: RcLogsFetchReply = {
      ok: obj.ok === true,
    }
    if (typeof obj.path === 'string') reply.path = obj.path
    if (Array.isArray(obj.lines)) {
      reply.lines = obj.lines.filter((s): s is string => typeof s === 'string')
    }
    if (typeof obj.truncated === 'boolean') reply.truncated = obj.truncated
    if (typeof obj.error === 'string') reply.error = obj.error
    return { kind: 'logs_fetch_reply', reply }
  }
  return null
}

/**
 * Pure helper: given the number of attempts already made (0-indexed
 * — i.e. `0` means the first retry hasn't fired yet), return the
 * delay before the next attempt. Always returns a positive delay —
 * after `RC_RECONNECT_LADDER_MS` is exhausted, falls back to
 * `RC_RECONNECT_STEADY_MS` (8 s) forever. The operator cancels by
 * settling the in-flight transfer (Cancel button), tearing down the
 * session (Disconnect), or closing the page. rc.23 — was previously
 * `null` after 6 attempts, which surfaced "budget exhausted" to the
 * field; field repro on PC50045 made it clear that operators on
 * corporate AV-protected hosts need indefinite retry.
 *
 * Exported for unit testing. Called by `scheduleReconnect()` inside
 * the composable; production code should not need this directly.
 */
export function nextReconnectDelayMs(attempt: number): number {
  if (attempt < 0) return RC_RECONNECT_LADDER_MS[0]
  if (attempt >= RC_RECONNECT_LADDER_MS.length) return RC_RECONNECT_STEADY_MS
  return RC_RECONNECT_LADDER_MS[attempt]
}

/** Pure helper: derive the host path to navigate to when the user
 *  double-clicks an entry in the files-browser drawer. Encodes two
 *  invariants that have each tripped a field bug:
 *
 *  1. **Roots view** (Drives on Windows / `/` on Unix) — `entry.name`
 *     is already an absolute path (e.g. `C:\` or `/`). The composable
 *     MUST drive into it directly; concatenating with the localised
 *     "Drives" label produces bogus paths like `Drives/C:\` (rc.15
 *     field repro 2026-05-07).
 *  2. **Inside a verbatim drive root** (`\\?\C:\`) — `Path::parent()`
 *     in the agent returns `None`, so a `currentParent === null`
 *     proxy mis-classifies the verbatim drive root as roots-view.
 *     The drawer must use an EXPLICIT `isRootsView` flag, set only
 *     when the navigateTo request was for empty/`/`/`~`. This helper
 *     takes that flag as input rather than re-deriving it
 *     (regression bug 2026-05-09: dbl-click `dev` after `C:\` shipped
 *     just `dev` to the agent → "canonicalising dev").
 *
 *  Path-separator heuristic: any drive-letter prefix or backslash in
 *  the current path → Windows; otherwise Unix. Trailing separator on
 *  the current path is detected so we don't double up.
 *
 *  Exported for unit-testing; the caller (RemoteControl.vue's
 *  `onEntryDblClick`) is a one-line wrapper that forwards `entry`,
 *  `currentDirPath`, and `isRootsView` directly.
 */
export function nextDirPath(
  entry: { name: string; is_dir: boolean },
  currentDirPath: string,
  isRootsView: boolean
): string | null {
  if (!entry.is_dir) return null
  if (isRootsView) {
    // Roots view: drive directly into the entry's name (already an
    // absolute path on Win / Unix).
    return entry.name
  }
  const trailingSep = /[\\/]$/.test(currentDirPath)
  // Win paths contain a drive-letter colon or a backslash. Anything
  // else is treated as Unix (forward-slash separator).
  const isWindows =
    /^[A-Za-z]:[\\\/]/.test(currentDirPath) || currentDirPath.includes('\\')
  const sep = trailingSep ? '' : isWindows ? '\\' : '/'
  return currentDirPath + sep + entry.name
}

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

/** Which video transport the viewer prefers for inbound frames.
 *  - `webrtc`: classic WebRTC video track. The default. Works on
 *    every browser; pixels go through Chrome's chroma-subsampled
 *    (4:2:0) decode path for every codec.
 *  - `data-channel-vp9-444`: VP9 profile 1 (8-bit 4:4:4) frames over
 *    an RTCDataChannel named `video-bytes`, decoded with WebCodecs
 *    `VideoDecoder` and painted to a canvas. Bypasses the WebRTC
 *    pipeline's 4:2:0 enforcement so screen-content text stays
 *    crisp. Requires `VideoDecoder.isConfigSupported({codec:
 *    'vp09.01.10.08'})` and an agent that advertises
 *    `data-channel-vp9-444` in its `AgentCaps.transports`. Falls
 *    back to `webrtc` silently when either side lacks support.
 *    Takes effect on the next `connect()`. */
export type RcVideoTransport = 'webrtc' | 'data-channel-vp9-444'

const VIDEO_TRANSPORT_STORAGE_KEY = 'roomler-rc-video-transport'

function readStoredVideoTransport(): RcVideoTransport {
  try {
    const raw = globalThis.localStorage?.getItem(VIDEO_TRANSPORT_STORAGE_KEY)
    if (raw === 'data-channel-vp9-444' || raw === 'webrtc') return raw
  } catch {
    /* privacy mode → default */
  }
  return 'webrtc'
}

function persistVideoTransport(t: RcVideoTransport) {
  try {
    globalThis.localStorage?.setItem(VIDEO_TRANSPORT_STORAGE_KEY, t)
  } catch {
    /* best-effort */
  }
}

/** Feature-detect VP9 profile 1 (8-bit 4:4:4) decode via WebCodecs.
 *  Returns `false` synchronously when the browser has no
 *  `VideoDecoder` at all; otherwise calls `isConfigSupported` and
 *  awaits the answer. Codec string is the WebCodecs canonical form
 *  for VP9 profile 1, bit depth 8 (`vp09.<profile>.<level>.<bit>`).
 *
 *  Exported for tests so the codec string is locked alongside the
 *  worker's `VideoDecoder.configure` call. */
export async function isVp9_444DecodeSupported(): Promise<boolean> {
  const g = globalThis as unknown as {
    VideoDecoder?: { isConfigSupported?: (cfg: { codec: string }) => Promise<{ supported?: boolean }> }
  }
  const isConfigSupported = g.VideoDecoder?.isConfigSupported
  if (typeof isConfigSupported !== 'function') return false
  try {
    const res = await isConfigSupported({ codec: 'vp09.01.10.08' })
    return res?.supported === true
  } catch {
    return false
  }
}

/** Wire-format constants for the `video-bytes` DataChannel. The label
 *  must match the agent's `on_data_channel` arm at peer.rs:494. The
 *  channel is reliable + ordered because (a) SCTP is doing the
 *  reassembly anyway and (b) dropping a P-frame would force the
 *  worker to wait for the next IDR — far worse than a few ms of
 *  retransmit latency. */
export const VP9_444_DC_LABEL = 'video-bytes'
export const VP9_444_DC_OPTIONS: RTCDataChannelInit = { ordered: true }

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

/**
 * rc.19: optional argument carrying the current agent's reactive
 * record so the composable can read `capabilities.files` (file-DC
 * v3 cap list including `"resume"`) without a separate Pinia
 * dependency. `RemoteControl.vue` passes its `agent: Ref<Agent>`;
 * tests + older callers can omit it and `supportsResume` falls
 * back to false (legacy rc.18 fail-fast upload semantics).
 */
export function useRemoteControl(agent?: Ref<Agent | null>) {
  const ws = useWsStore()
  const phase = ref<RcPhase>('idle')

  /**
   * rc.19: resume opt-in gate. True only when the agent has
   * advertised `"resume"` in `capabilities.files`. Browsers that
   * see no resume cap (rc.18 agents, or rc.19 agents with browse
   * disabled) keep the legacy direct-pump-with-fail-fast path.
   */
  const supportsResume: ComputedRef<boolean> = computed(() => {
    const files = agent?.value?.capabilities?.files
    return Array.isArray(files) && files.includes('resume')
  })
  const error = ref<string | null>(null)
  const sessionId = ref<string | null>(null)
  /**
   * Auto-reconnect state. `lastConnectArgs` remembers the user's
   * original `connect(agentId, permissions)` call so a reconnect can
   * re-establish the same session against the same agent without
   * the operator hitting Connect again. `reconnectAttempt` is exposed
   * so the viewer can render "Reconnecting (3/6)..." in the toolbar
   * — silent retries are confusing when an operator is watching the
   * stream go dark. `reconnectTimer` is private; managed by
   * scheduleReconnect / cancelReconnect.
   */
  let lastConnectArgs: { agentId: string; permissions: string } | null = null
  const reconnectAttempt = ref(0)
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  /**
   * Whether the agent has signalled (over the `control` data channel)
   * that the host's input desktop has transitioned to `winsta0\Winlogon`
   * (lock screen / UAC consent / secure attention sequence). The
   * lock-overlay frame on the video track already shows the visual
   * state; this flag is a separate machine-readable signal so the
   * viewer can render a toolbar badge that's visible even when the
   * video element is scrolled out of view.
   *
   * Stays false on agents older than 0.2.2 (which never emit the
   * message); the flag remains coherent because falling back to
   * always-false matches the pre-overlay behaviour for those agents.
   */
  const hostLocked = ref(false)
  /**
   * Current input desktop name reported by the SYSTEM-context
   * worker (agents 0.3.0+) via the `rc:desktop_changed` control-DC
   * message. `'Default'` is the normal interactive desktop;
   * `'Winlogon'` (or `'Screen-saver'`, etc.) means the operator
   * is on a secure desktop. Older agents never emit the message;
   * the ref stays at `'Default'` and the viewer renders no
   * secondary chip.
   */
  const currentDesktop = ref<string>('Default')
  /**
   * rc.23 — diagnostic surface for the `rc:logs-fetch` round-trip.
   * `agentLogs` holds the last reply (or null if no fetch has run);
   * `agentLogsLoading` flips true while a request is in flight.
   * Operator drives via `fetchAgentLogs(linesCount)` from the UI.
   */
  const agentLogs = ref<RcLogsFetchReply | null>(null)
  const agentLogsLoading = ref(false)
  /**
   * Single-flight promise resolver — when set, the next
   * `rc:logs-fetch.reply` arriving over the control DC resolves it.
   * Set inside `fetchAgentLogs()`, cleared in the onmessage handler.
   * Subsequent rapid calls cancel the pending promise (reply may
   * still arrive and is dropped silently).
   */
  let pendingLogsResolver: ((reply: RcLogsFetchReply) => void) | null = null
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
  /** Preferred video transport. `webrtc` is the legacy default; the
   *  user opts in to `data-channel-vp9-444` when they want crystal-
   *  clear 4:4:4 text rendering. The actual negotiation is done on
   *  the agent: this ref is only consulted at `connect()` time, the
   *  agent reads `preferred_transport` and intersects it with its
   *  own `AgentCaps.transports`. Persisted per-browser. */
  const videoTransport = ref<RcVideoTransport>(readStoredVideoTransport())
  /** Whether VP9 profile 1 (8-bit 4:4:4) decode is supported on this
   *  browser. Resolved asynchronously by `isVp9_444DecodeSupported()`
   *  in `connect()` (and re-checked once on first composable use, so
   *  the UI can disable the toolbar toggle when unsupported). The UI
   *  reads this; an unset/false value means the data-channel transport
   *  is unavailable regardless of the user's stored preference. */
  const vp9_444Supported = ref<boolean>(false)
  // Kick off the async probe immediately. We only need the answer at
  // connect() time so the await isn't latency-critical, but resolving
  // it eagerly lets the UI disable the toolbar toggle on browsers
  // that lack VP9 profile 1 support.
  void isVp9_444DecodeSupported().then((ok) => { vp9_444Supported.value = ok })
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
  /** `true` once the WebCodecs transform is successfully installed
   *  on the receiver AND we're committed to painting to the canvas.
   *  Stays `false` when we fall back (HEVC, missing API, worker
   *  ctor failure, transferControlToOffscreen throw). The VIEW
   *  reads this (not the `renderPath` preference) to decide which
   *  element to mount — so an HEVC session under renderPath='webcodecs'
   *  correctly renders the `<video>` rather than a permanent black
   *  canvas. */
  const webcodecsActive = ref(false)

  // Phase Y.3: VP9-444 over DataChannel pipeline. Independent of the
  // RTCRtpScriptTransform path above — uses its OWN worker
  // (rc-vp9-444-worker.ts) fed off `video-bytes` DC binary messages.
  let vp9_444Worker: Worker | null = null
  /** `true` once the worker has been spun up and the DC opened. The
   *  view (Y.4) reads this to swap a `<canvas>` in for the `<video>`
   *  element, mirroring how `webcodecsActive` drives the WebCodecs
   *  path. Stays `false` when the user didn't opt in OR the agent
   *  doesn't honour the transport (no DC ever arrives → flag never
   *  flips). */
  const vp9_444Active = ref(false)
  /** Number of decoded VP9-444 frames so far. Surfaced to the view
   *  for diagnostics and used by tests to assert end-to-end decode
   *  succeeded. */
  const vp9_444FramesDecoded = ref(0)
  /** The visible `<canvas>` the view paints VP9-444 frames into. The
   *  view writes this on mount; the composable picks it up and
   *  posts `init-canvas` to the worker. Null until the view
   *  provides a canvas — Y.3 ships without view-side wiring, so
   *  bytes flow + decode happens against a synthetic OffscreenCanvas
   *  instead. */
  const vp9_444CanvasEl = ref<HTMLCanvasElement | null>(null)

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

  // ---- File-DC registry (shared across all `files` channel transfers) ----
  // The `files` DC carries multiple concurrent kinds of work in 0.3.0+:
  // single-file uploads, multi-file upload queues (Phase 1), single-file
  // downloads (Phase 2), folder downloads (Phase 4), and dir-list requests
  // (Phase 3). All of them are demuxed from a single persistent
  // `onmessage` listener attached at DC creation time (see channels.files
  // setup further down). Per-call listener-add was the pattern in 0.2.x;
  // it works for one transfer at a time but doesn't compose with
  // concurrent up + down or a queued multi-upload.
  //
  // Each entry tracks a state (`pending` → `settled`); only the first
  // transition wins, so a `files:cancel` racing a `files:complete` /
  // `files:eof` doesn't double-resolve the Promise.
  type UploadResolve = (result: { path: string; bytes: number }) => void
  type DownloadResolve = (result: { name: string; bytes: number }) => void
  // FileSystemWritableFileStream is the showSaveFilePicker writable
  // (Chrome / Edge / Safari 17+). Older TS lib targets miss the type;
  // we keep the structural shape we actually use to avoid a lib bump.
  type SaveWritable = {
    write: (data: Uint8Array | ArrayBuffer) => Promise<void>
    close: () => Promise<void>
    abort: (reason?: unknown) => Promise<void>
  }
  type DownloadEntry = {
    kind: 'download'
    status: 'pending' | 'settled'
    resolve: DownloadResolve
    reject: (err: Error) => void
    // Sink: either a streaming writable (Chrome) OR a Blob accumulator
    // (Firefox / Safari < 17). Decided at downloadFile() time and
    // populated when files:offer arrives.
    saveMode: 'stream' | 'blob' | 'pending'
    writable: SaveWritable | null
    blobs: BlobPart[]
    name: string
    suggestedName?: string
    bytesReceived: number
    expectedSize: number | null
    mime?: string
  }
  /**
   * Upload entry. rc.19 carries enough context to re-pump after a
   * DC drop:
   * - `bytesAcked` mirrors the agent's last `files:progress` so
   *   `files:resume` knows the safest offset to request.
   * - `file` / `relPath` / `destPath` survive the original
   *   closure-only state from rc.18's `uploadOne` so the resume
   *   loop can call `innerPump` again without rebuilding the
   *   call-site context.
   * - `status: 'pending-resume'` is the in-between state set by
   *   the DC-close handler when the agent has the resume cap;
   *   the wrapper transitions it back to `'pending'` after
   *   `files:resumed` lands.
   */
  type UploadEntry = {
    kind: 'upload'
    status: 'pending' | 'pending-resume' | 'settled'
    resolve: UploadResolve
    reject: (err: Error) => void
    bytesAcked: number
    file: File
    relPath?: string
    destPath?: string
  }
  type RegistryEntry = UploadEntry | DownloadEntry
  const filesRegistry = new Map<string, RegistryEntry>()

  /**
   * rc.19: awaiters for `files:resumed { id, accepted_offset }`
   * replies during the resume handshake window. Separate from
   * `filesRegistry` because the resume wrapper needs the
   * resumed reply BEFORE the entry transitions back to `'pending'`
   * — routing through `filesRegistry.get(id)` would race with the
   * close-handler's `'pending-resume'` patch. Shape mirrors the
   * `pendingDirRequests` pattern used for `files:dir-list`.
   */
  type ResumeWaiter = {
    resolve: (acceptedOffset: number) => void
    reject: (err: Error) => void
    timer: ReturnType<typeof setTimeout>
  }
  const pendingResumePromises = new Map<string, ResumeWaiter>()
  // The browser-side demux contract: while a download `files:offer` is
  // active, every binary frame on the DC belongs to that id. There can
  // only be one active outgoing transfer at a time (server enforces);
  // we mirror that here so binary chunks find the right writable.
  let activeDownloadId: string | null = null
  // Settle an entry exactly once. Returns true if THIS call won the
  // transition; false if the entry was already settled. The caller uses
  // the return value to skip duplicate resolve / reject.
  function settleEntry(id: string): RegistryEntry | null {
    const entry = filesRegistry.get(id)
    if (!entry || entry.status === 'settled') return null
    entry.status = 'settled'
    filesRegistry.delete(id)
    return entry
  }

  // Reactive list of in-flight + recently-finished file transfers. The
  // Transfers chip in RemoteControl.vue binds to this. Entries auto-prune
  // after 10 s in a terminal state so the panel doesn't grow unboundedly
  // over a long session.
  type TransferStatus =
    | 'queued'
    | 'running'
    /** rc.19: DC closed mid-upload but the agent has the resume cap;
     *  the wrapper is waiting for the WebRTC peer to reconnect so
     *  it can issue `files:resume`. Operator sees "Reconnecting N/6". */
    | 'reconnecting'
    | 'complete'
    | 'error'
    | 'cancelled'
  interface Transfer {
    id: string
    kind: 'upload' | 'download'
    name: string
    bytes: number
    total: number | null
    status: TransferStatus
    error?: string
  }
  const transfers = ref<Transfer[]>([])
  function pushTransfer(t: Transfer) {
    transfers.value = [...transfers.value, t]
  }
  function patchTransfer(id: string, patch: Partial<Transfer>) {
    transfers.value = transfers.value.map((t) => (t.id === id ? { ...t, ...patch } : t))
    if (patch.status === 'complete' || patch.status === 'error' || patch.status === 'cancelled') {
      // Auto-prune after 10 s in a terminal state. rc.19 'reconnecting'
      // is explicitly NOT terminal — the wrapper transitions out of it
      // either back to 'running' (resume accepted) or to 'error' (6
      // attempts exhausted), at which point this branch fires again.
      setTimeout(() => {
        transfers.value = transfers.value.filter((t) => t.id !== id)
      }, 10_000)
    }
  }

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

  /** Type literal text on the remote host. Used by the on-screen
   *  mobile keyboard and the IME composition path: the agent's
   *  `enigo.text()` invokes the OS Unicode-typing API, so emoji /
   *  CJK / accented Latin all round-trip without any HID-code
   *  mapping on the browser side. Safe to call when the input
   *  channel isn't open — silent drop. */
  function sendKeyText(text: string) {
    if (!text) return
    sendInput({ t: 'key_text', text })
  }

  /** Send a HID key event. Used by the mobile keyboard's special-
   *  key toolbar (Esc/Tab/Enter/Backspace/arrows + modifier keys).
   *  Mirrors the wire shape of the regular physical-key path. Pass
   *  the same `code` / `down` / `mods` triple as `decideKeyAction`
   *  produces. Safe to call when the input channel isn't open.
   *
   *  `mods` bitfield: 0x01 = Ctrl, 0x02 = Shift, 0x04 = Alt,
   *  0x08 = Meta/Win — matches `kbdCodeToHid` callers throughout
   *  the codebase. */
  function sendKey(code: number, down: boolean, mods: number = 0) {
    sendInput({ t: 'key', code, down, mods })
  }

  /**
   * rc.23 — request a tail of the agent's log file over the control
   * DC. Sends `rc:logs-fetch { lines }` and awaits the matching
   * `rc:logs-fetch.reply`. Single-flight: a second call while one is
   * pending cancels the prior promise (the late reply is dropped).
   *
   * Returns the reply or rejects with a clear error when the control
   * DC isn't open / the timeout fires / agent is too old to support
   * the message. Newer agents reply within ~50 ms for the default
   * 500-line tail; the 8 s timeout is generous for slow disks.
   */
  function fetchAgentLogs(lines = 500): Promise<RcLogsFetchReply> {
    return new Promise((resolve, reject) => {
      const ch = channels.control
      if (!ch || ch.readyState !== 'open') {
        reject(new Error('control DC not open — not connected to agent'))
        return
      }
      // Cancel any prior in-flight request — late reply is dropped.
      const prevResolver = pendingLogsResolver
      if (prevResolver !== null) {
        // Resolve the prior promise with a synthetic error so its
        // caller doesn't hang forever.
        prevResolver({ ok: false, error: 'superseded by a newer fetch' })
      }
      agentLogsLoading.value = true
      // `isActive` is the single source of truth for "this request is
      // still awaiting a reply." Avoids the rc.23-first-cut bug where
      // the timer compared `pendingLogsResolver === resolve` but the
      // pendingLogsResolver had been reassigned to a wrapper closure,
      // so the timer body's guard always failed and the spinner spun
      // forever when the agent didn't respond (old agent unaware of
      // `rc:logs-fetch`, or DC half-open after a peer drop).
      let isActive = true
      // rc.23 hotfix #2 — 30 s timeout (was 8 s). PC50045 field
      // report: log fetch timing out even on rc.23 agent. Agent's
      // file read might be ESET-intercepted (the agent's tracing
      // log is itself a file that ESET scans on read). 8 s gave too
      // little budget; 30 s matches the "operator clicks Refresh
      // and waits a beat" experience.
      const timer = setTimeout(() => {
        if (!isActive) return
        isActive = false
        pendingLogsResolver = null
        agentLogsLoading.value = false
        reject(
          new Error(
            'rc:logs-fetch timed out after 30 s — agent might be on rc.22 or older, or its log read is being held by the AV scanner'
          )
        )
      }, 30000)
      pendingLogsResolver = (reply) => {
        if (!isActive) return
        isActive = false
        clearTimeout(timer)
        pendingLogsResolver = null
        agentLogsLoading.value = false
        resolve(reply)
      }
      try {
        ch.send(JSON.stringify({ t: 'rc:logs-fetch', lines }))
      } catch (e) {
        if (!isActive) return
        isActive = false
        clearTimeout(timer)
        pendingLogsResolver = null
        agentLogsLoading.value = false
        reject(e instanceof Error ? e : new Error(String(e)))
      }
    })
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

  /** Switch video transport. Only takes effect on the next `connect()`
   *  — the choice is baked into the rc:session.request payload. If the
   *  caller asks for `data-channel-vp9-444` but `vp9_444Supported` is
   *  false (older browser, or the async probe hasn't resolved yet),
   *  we still persist the preference so the toggle reflects the user
   *  intent; the actual transport negotiation falls back to webrtc
   *  on the agent side when its caps don't include the field. */
  function setVideoTransport(t: RcVideoTransport) {
    videoTransport.value = t
    persistVideoTransport(t)
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
    // Chrome (≤ 131 at least) installs RTCRtpScriptTransform on an
    // HEVC receiver without complaint but bypasses it — frames go
    // straight to the default decoder and our TransformStream never
    // sees them. Observed 2026-04-24 on Intel UHD + real HEVC track:
    // `receiver.getStats()` showed framesReceived + framesDecoded
    // climbing normally while the worker's `first-encoded-frame`
    // message never fired. Until Chrome closes that gap, auto-fall-
    // back to the `<video>` path for HEVC so the user sees video
    // instead of a black canvas.
    const sdpCodec = codecFromSdp(pc?.currentRemoteDescription?.sdp)
    const receiverCodec = shortCodecFromReceiver(receiver)
    const codec = sdpCodec ?? receiverCodec
    if (codec === 'h265') {
      console.warn(
        '[rc] webcodecs path skipped — Chrome does not forward HEVC frames to RTCRtpScriptTransform. Falling back to <video>. Use the Codec toolbar to force H.264 for a guaranteed WebCodecs path.',
      )
      return false
    }
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
        // Chrome's RTCRtpScriptTransform silently drops frames for
        // some codec/version combos (HEVC ≤ Chrome 131, also H.264
        // in some 2026-04-26 builds). The default decoder still gets
        // the frames via our pipeThrough → writable, so the
        // <video> element would render fine — we just need to swap
        // the DOM. Tear down the worker; webcodecsActive flips to
        // false; the view's `isWebCodecsRender` computed reverts;
        // Vue mounts <video> and the existing srcObject watcher
        // wires the stream. No reconnect needed.
        console.warn('[rc] webcodecs watchdog fired — auto-fallback to <video>', msg)
        stopWebCodecsPath()
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
    webcodecsActive.value = true
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
      if (ticks > 5 || !webcodecsActive.value) {
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
    webcodecsActive.value = false
    if (!webcodecsWorker) return
    try { webcodecsWorker.postMessage({ type: 'close' }) } catch { /* ignore */ }
    try { webcodecsWorker.terminate() } catch { /* ignore */ }
    webcodecsWorker = null
  }

  /** Boot the VP9-444 worker, open the `video-bytes` DataChannel, and
   *  forward incoming binary messages to the worker. Called from
   *  `connect()` only when the browser opted in to
   *  `data-channel-vp9-444` AND VP9 profile 1 decode is supported.
   *  Idempotent — a second call while a worker exists is a no-op
   *  (wraps the existing channel + worker pair).
   *
   *  The worker self-decodes against an `OffscreenCanvas`. For Y.3
   *  the canvas is synthetic (created here, never displayed); Y.4
   *  hooks the view's `<canvas>` element via
   *  `vp9_444CanvasEl` + a `transferControlToOffscreen()` swap.
   *  Bytes still flow + frames still decode in the synthetic case,
   *  which is what e2e + integration tests assert against.
   *
   *  Returns the worker handle so tests can drive it directly. */
  function startVp9_444Path(): Worker | null {
    if (vp9_444Worker) return vp9_444Worker
    if (!pc) return null
    let worker: Worker
    try {
      worker = new Worker(
        new URL('../workers/rc-vp9-444-worker.ts', import.meta.url),
        { type: 'module' },
      )
    } catch (err) {
      console.warn('[rc] vp9-444 worker construction failed', err)
      return null
    }
    worker.onmessage = (ev) => {
      const msg = ev.data as Record<string, unknown> | undefined
      if (!msg || typeof msg.type !== 'string') return
      if (msg.type === 'first-frame'
        && typeof msg.width === 'number'
        && typeof msg.height === 'number') {
        mediaIntrinsicW.value = msg.width
        mediaIntrinsicH.value = msg.height
        vp9_444FramesDecoded.value = Math.max(vp9_444FramesDecoded.value, 1)
        console.info('[rc] vp9-444 first frame', msg.width, 'x', msg.height)
      } else if (msg.type === 'decoder-configured') {
        console.info('[rc] vp9-444 decoder configured', msg.codec)
      } else if (msg.type === 'decoder-error'
        || msg.type === 'decoder-configure-error'
        || msg.type === 'decode-error') {
        console.warn('[rc] vp9-444 worker', msg.type, msg.error)
      } else if (msg.type === 'frame-rejected') {
        console.warn('[rc] vp9-444 frame rejected', msg)
      } else if (msg.type === 'frame-decoded') {
        // Worker emits this for every decoded frame after the first.
        // Driven by the worker's `output` callback, used by tests +
        // view-side diagnostics.
        vp9_444FramesDecoded.value++
      }
    }
    // Synthetic OffscreenCanvas — keeps the worker fully wired even
    // without a view-side canvas. Y.4 swaps in the visible canvas
    // via vp9_444CanvasEl watcher below.
    try {
      const synthetic = new OffscreenCanvas(2, 2)
      worker.postMessage({ type: 'init-canvas', canvas: synthetic }, [synthetic])
    } catch (err) {
      console.warn('[rc] vp9-444: synthetic OffscreenCanvas init failed', err)
      try { worker.terminate() } catch { /* ignore */ }
      return null
    }
    // Open the DC. Forward binary messages straight through to the
    // worker as ArrayBuffer chunks (transferred, not copied).
    let dc: RTCDataChannel
    try {
      dc = pc.createDataChannel(VP9_444_DC_LABEL, VP9_444_DC_OPTIONS)
    } catch (err) {
      console.warn('[rc] vp9-444 DC creation failed', err)
      try { worker.terminate() } catch { /* ignore */ }
      return null
    }
    dc.binaryType = 'arraybuffer'
    dc.onmessage = (ev) => {
      if (!(ev.data instanceof ArrayBuffer)) return
      try {
        worker.postMessage({ type: 'chunk', bytes: ev.data }, [ev.data])
      } catch (err) {
        console.warn('[rc] vp9-444 worker post failed', err)
      }
    }
    dc.onopen = () => {
      console.info('[rc] vp9-444 DC opened')
    }
    dc.onclose = () => {
      console.info('[rc] vp9-444 DC closed')
    }
    channels.videoBytes = dc
    vp9_444Worker = worker
    vp9_444Active.value = true
    return worker
  }

  function stopVp9_444Path() {
    vp9_444Active.value = false
    vp9_444FramesDecoded.value = 0
    if (!vp9_444Worker) return
    try { vp9_444Worker.postMessage({ type: 'close' }) } catch { /* ignore */ }
    try { vp9_444Worker.terminate() } catch { /* ignore */ }
    vp9_444Worker = null
  }

  /** When the view mounts a real `<canvas>`, swap it in for the
   *  synthetic OffscreenCanvas the worker started with. The worker
   *  treats `init-canvas` as idempotent — second call replaces the
   *  paint target. */
  watch(vp9_444CanvasEl, (el) => {
    if (!el || !vp9_444Worker) return
    try {
      const off = el.transferControlToOffscreen()
      vp9_444Worker.postMessage({ type: 'init-canvas', canvas: off }, [off])
    } catch (err) {
      console.warn('[rc] vp9-444: transferControlToOffscreen failed', err)
    }
  })

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
    cancelReconnect()
    lastConnectArgs = null
    teardown()
  }

  /**
   * Cancel any pending reconnect timer and reset the attempt counter.
   * Called from `failWith` (terminal error), `disconnect` (user-
   * initiated teardown), and on every successful 'connected'
   * transition (so a stable session that later fails starts the
   * ladder from 250 ms again, not from where it left off).
   */
  function cancelReconnect() {
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer)
      reconnectTimer = null
    }
    reconnectAttempt.value = 0
  }

  /**
   * Schedule the next reconnect attempt according to
   * `RC_RECONNECT_LADDER_MS`. Cancels any prior schedule (no
   * stacking). After `RC_RECONNECT_LADDER_MS.length` attempts have
   * elapsed without a 'connected' transition resetting the counter,
   * gives up and calls `failWith` so the operator sees the failure
   * instead of a hung "reconnecting" UI.
   *
   * The PC is torn down at schedule time (not retry time) so any
   * lingering ICE / track listeners don't fire on the dead PC while
   * the timer is pending.
   */
  function scheduleReconnect() {
    // Replace any prior schedule.
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer)
      reconnectTimer = null
    }
    // Without the original connect args we can't retry.
    if (!lastConnectArgs) {
      failWith('peer connection failed')
      return
    }
    const attemptIdx = reconnectAttempt.value
    // rc.23 — nextReconnectDelayMs always returns a positive delay
    // now; the loop only exits when the operator clicks Disconnect
    // (which sets lastConnectArgs = null and falls into the failWith
    // above) or the peer transitions back to 'connected'. Removes
    // the "budget exhausted" terminal that frustrated operators on
    // corporate AV-protected hosts where the agent gets killed and
    // restarted repeatedly during large uploads.
    const delay = nextReconnectDelayMs(attemptIdx)
    reconnectAttempt.value = attemptIdx + 1
    phase.value = 'reconnecting'
    teardown()
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null
      const args = lastConnectArgs
      if (!args) return
      // `connect()` resets phase / sessionId on entry; the early-
      // return guard for non-{idle,closed,error} states is OK
      // because we set 'reconnecting' which falls outside those.
      // We `await` via .catch so a synchronous throw inside connect
      // chains into another reconnect attempt instead of bubbling
      // unhandled.
      void connect(args.agentId, args.permissions, /* isReconnect */ true).catch(() => {
        scheduleReconnect()
      })
    }, delay)
  }

  function teardown() {
    stopStatsPoll()
    stopWebCodecsPath()
    stopVp9_444Path()
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
    hostLocked.value = false
    currentDesktop.value = 'Default'
  }

  async function connect(
    agentId: string,
    permissions = 'VIEW | INPUT | CLIPBOARD',
    isReconnect = false,
  ) {
    // The reconnect path is allowed to drive `connect` while phase ==
    // 'reconnecting'; user-initiated calls must still be blocked from
    // re-entering an active session.
    if (
      phase.value !== 'idle'
      && phase.value !== 'closed'
      && phase.value !== 'error'
      && !(isReconnect && phase.value === 'reconnecting')
    ) {
      return // already active
    }
    // Capture the original call so a later 'failed' can replay it.
    // Don't clobber on an isReconnect call — that path already has
    // the right args from the original user click.
    if (!isReconnect) {
      lastConnectArgs = { agentId, permissions }
      // Fresh user-initiated connect → reset reconnect state.
      cancelReconnect()
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
      if (state === 'connected') {
        phase.value = 'connected'
        // A successful connection (whether the initial one or a
        // mid-ladder retry) resets the attempt counter. Without this
        // a long-lived session that drops once at hour 5 would jump
        // straight to attempt 4 and use a 4 s first delay instead of
        // 250 ms.
        cancelReconnect()
      } else if (state === 'failed') {
        // M3 hand-off / desktop transition / network blip. Replace
        // the previous immediate-failWith with the auto-reconnect
        // ladder so the operator doesn't have to F5 + reconnect
        // every time the host briefly goes dark. Only 'failed'
        // triggers retry; 'disconnected' is transient (ICE
        // checking) and recovers on its own.
        scheduleReconnect()
      } else if (state === 'closed' && phase.value !== 'error' && phase.value !== 'closed' && phase.value !== 'reconnecting') {
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

    // Persistent listener on the `files` DC. Demuxes every control
    // message by id and dispatches to the registry entry that owns
    // the transfer. Single attach point: replaces the per-call
    // addEventListener pattern from 0.2.x. See `filesRegistry` doc
    // comment for the lifecycle contract.
    //
    // String frames are JSON control messages (files:offer, eof,
    // complete, error, progress, accepted, dir-list, dir-error).
    // Binary frames are download chunks routed to the
    // `activeDownloadId`'s registry entry per the demux contract
    // (one active outgoing transfer at a time).
    channels.files.onmessage = (ev) => {
      if (typeof ev.data !== 'string') {
        // Binary frame — route to the active download's writable
        // or Blob accumulator. If no active download, drop (would
        // be a protocol violation; agent shouldn't send binaries
        // without a preceding files:offer).
        if (!activeDownloadId) return
        const entry = filesRegistry.get(activeDownloadId)
        if (!entry || entry.kind !== 'download' || entry.status === 'settled') return
        // ev.data may be ArrayBuffer or Blob depending on the DC's
        // binaryType. webrtc-rs DCs default to ArrayBuffer.
        const data = ev.data as ArrayBuffer | Blob
        if (data instanceof ArrayBuffer) {
          appendDownloadChunk(entry, data)
        } else if (data instanceof Blob) {
          // Async path; we don't await — entries are kept in arrival
          // order via the same await chain.
          void data.arrayBuffer().then((buf) => appendDownloadChunk(entry, buf))
        }
        return
      }
      let msg: {
        t?: string
        id?: string
        req_id?: string
        name?: string
        size?: number | null
        mime?: string
        path?: string
        parent?: string | null
        entries?: DirEntry[]
        bytes?: number
        message?: string
        /** rc.19 files:resumed reply — server-authoritative offset
         *  the browser should re-pump from. */
        accepted_offset?: number
      }
      try {
        msg = JSON.parse(ev.data)
      } catch {
        return
      }
      const id = typeof msg.id === 'string' ? msg.id : ''
      // Directory listing replies are demuxed by req_id, not id.
      if (msg.t === 'files:dir-list') {
        const reqId = typeof msg.req_id === 'string' ? msg.req_id : ''
        const pending = settleDirRequest(reqId)
        if (pending) {
          pending.resolve({
            path: String(msg.path ?? ''),
            parent: typeof msg.parent === 'string' ? msg.parent : null,
            entries: Array.isArray(msg.entries) ? msg.entries : [],
          })
        }
        return
      } else if (msg.t === 'files:dir-error') {
        const reqId = typeof msg.req_id === 'string' ? msg.req_id : ''
        const pending = settleDirRequest(reqId)
        if (pending) {
          pending.reject(new Error(String(msg.message ?? 'agent dir error')))
        }
        return
      }
      if (msg.t === 'files:complete') {
        const entry = settleEntry(id)
        if (entry?.kind === 'upload') {
          patchTransfer(id, { status: 'complete', bytes: Number(msg.bytes ?? 0) })
          entry.resolve({ path: String(msg.path ?? ''), bytes: Number(msg.bytes ?? 0) })
        }
      } else if (msg.t === 'files:error') {
        const errMsg = String(msg.message ?? 'agent error')
        // rc.19: if a resume handshake is waiting on this id, route
        // the error THERE first. The wrapper falls back to a fresh
        // `files:begin` with a new id (see uploadOneResumable).
        // The original upload entry stays in `filesRegistry` so the
        // wrapper can rebind it.
        const waiter = pendingResumePromises.get(id)
        if (waiter) {
          clearTimeout(waiter.timer)
          pendingResumePromises.delete(id)
          waiter.reject(new Error(errMsg))
          return
        }
        // Errors can land for either an upload OR a download.
        const entry = settleEntry(id)
        if (!entry) return
        patchTransfer(id, { status: 'error', error: errMsg })
        if (entry.kind === 'upload') {
          entry.reject(new Error(errMsg))
        } else {
          // Download error: abort writable so Chrome auto-deletes
          // any partial file in the user's chosen save location.
          if (id === activeDownloadId) activeDownloadId = null
          if (entry.writable) {
            void entry.writable.abort(errMsg).catch(() => {})
          }
          entry.reject(new Error(errMsg))
        }
      } else if (msg.t === 'files:progress') {
        const bytes = Number(msg.bytes ?? 0)
        patchTransfer(id, { status: 'running', bytes })
        // rc.19: each progress envelope is a durable-bytes ack
        // (agent calls sync_data per 1 MiB before emitting).
        // Update the upload entry so a future resume request
        // claims the right offset.
        const entry = filesRegistry.get(id)
        if (entry?.kind === 'upload') {
          entry.bytesAcked = bytes
        }
      } else if (msg.t === 'files:resumed') {
        // rc.19: agent → browser reply confirming the byte offset
        // from which to re-pump. Routed via pendingResumePromises
        // (NOT filesRegistry) — the entry is currently in
        // 'pending-resume' state and we hand control back to the
        // resume wrapper via this waiter.
        const waiter = pendingResumePromises.get(id)
        if (waiter) {
          clearTimeout(waiter.timer)
          pendingResumePromises.delete(id)
          waiter.resolve(Number(msg.accepted_offset ?? 0))
        }
      } else if (msg.t === 'files:accepted') {
        patchTransfer(id, { status: 'running' })
      } else if (msg.t === 'files:offer') {
        const entry = filesRegistry.get(id)
        if (!entry || entry.kind !== 'download' || entry.status === 'settled') return
        entry.name = String(msg.name ?? entry.suggestedName ?? 'download.bin')
        entry.expectedSize = typeof msg.size === 'number' ? msg.size : null
        entry.mime = typeof msg.mime === 'string' ? msg.mime : undefined
        activeDownloadId = id
        patchTransfer(id, {
          status: 'running',
          name: entry.name,
          total: entry.expectedSize,
        })
        // Resolve the save-mode: prefer streaming when the browser
        // supports showSaveFilePicker AND the caller didn't preselect
        // a Blob path. The picker MUST have been opened by the
        // caller (a synchronous user gesture is required); by the
        // time files:offer arrives we already have the writable in
        // entry.writable if the picker resolved.
        if (entry.saveMode === 'pending') {
          // No picker was set up; fall back to Blob accumulator.
          entry.saveMode = 'blob'
        }
      } else if (msg.t === 'files:eof') {
        const entry = filesRegistry.get(id)
        if (!entry || entry.kind !== 'download' || entry.status === 'settled') return
        const totalBytes = Number(msg.bytes ?? entry.bytesReceived)
        if (id === activeDownloadId) activeDownloadId = null
        // Finalize: close the writable (Chrome streaming) or trigger
        // the anchor download (Blob fallback).
        void finalizeDownload(entry, totalBytes).then(
          () => {
            const settled = settleEntry(id)
            if (settled?.kind === 'download') {
              patchTransfer(id, { status: 'complete', bytes: totalBytes })
              settled.resolve({ name: entry.name, bytes: totalBytes })
            }
          },
          (err: unknown) => {
            const settled = settleEntry(id)
            if (settled?.kind === 'download') {
              const errMsg = err instanceof Error ? err.message : String(err)
              patchTransfer(id, { status: 'error', error: errMsg })
              settled.reject(new Error(errMsg))
            }
          }
        )
      }
    }
    // DC close handler. Pre-rc.19: every pending transfer is
    // settled with "channel closed" and the operator has to manually
    // retry. rc.19: when the agent has the resume cap, UPLOAD
    // entries are deferred to 'pending-resume' state so the
    // `uploadOneResumable` wrapper can issue `files:resume` after
    // the WebRTC peer reconnects (handled by `scheduleReconnect`).
    // Downloads still fail-fast — host → browser resume is future
    // work.
    channels.files.onclose = () => {
      activeDownloadId = null
      // Reject any in-flight resume handshakes — the new DC will
      // get a fresh waiter.
      for (const [id, w] of Array.from(pendingResumePromises.entries())) {
        clearTimeout(w.timer)
        pendingResumePromises.delete(id)
        w.reject(new Error('files channel closed mid-resume'))
      }
      const errMsg = 'files channel closed'
      for (const id of Array.from(filesRegistry.keys())) {
        const entry = filesRegistry.get(id)
        if (!entry || entry.status === 'settled') continue
        if (entry.kind === 'upload' && supportsResume.value) {
          // Defer settle — the uploadOneResumable wrapper is awaiting
          // the next `phase === 'connected'` and will issue
          // `files:resume`. Transition to 'pending-resume' so a
          // late files:complete on a stale DC can't double-settle.
          entry.status = 'pending-resume'
          patchTransfer(id, { status: 'reconnecting', error: 'waiting for reconnect' })
          continue
        }
        const settled = settleEntry(id)
        if (!settled) continue
        patchTransfer(id, { status: 'error', error: errMsg })
        if (settled.kind === 'upload') {
          settled.reject(new Error(errMsg))
        } else if (settled.kind === 'download') {
          if (settled.writable) {
            void settled.writable.abort(errMsg).catch(() => {})
          }
          settled.reject(new Error(errMsg))
        }
      }
    }

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
    // Agent → browser control messages. Recognised:
    //   - `rc:host_locked` (boolean) — the agent flips this on/off
    //     as `lock_state.rs` observes desktop transitions (0.2.3+).
    //   - `rc:desktop_changed` (string name) — the SYSTEM-context
    //     worker emits this after every `try_change_desktop`
    //     Switched, so the viewer shows e.g. "On Winlogon" while
    //     the operator drives the lock screen (0.3.0+).
    // Other variants (rc:dpi-change, rc:cursor-shape) layer on the
    // same parse-by-`t` switch. Unknown `t` values are dropped
    // silently; older agents emitted nothing here, so backward-
    // compat is automatic.
    channels.control.onmessage = (ev) => {
      // rc.23 hotfix — trace every inbound control envelope to the
      // browser console at debug level so the field can see, via
      // DevTools, exactly which messages the agent is sending. Helps
      // diagnose "rc:logs-fetch.reply never arrived" reports without
      // requiring an agent log fetch (which itself depends on the
      // round-trip working). Truncated to first 200 chars so a huge
      // logs payload doesn't blow up the console.
      if (typeof ev.data === 'string') {
        // eslint-disable-next-line no-console
        console.debug(
          '[rc:control] inbound:',
          ev.data.length > 200 ? ev.data.slice(0, 200) + '…' : ev.data
        )
      }
      const parsed = parseControlInbound(ev.data)
      if (parsed?.kind === 'host_locked') {
        hostLocked.value = parsed.locked
      } else if (parsed?.kind === 'desktop_changed') {
        currentDesktop.value = parsed.name
      } else if (parsed?.kind === 'logs_fetch_reply') {
        agentLogs.value = parsed.reply
        agentLogsLoading.value = false
        const resolve = pendingLogsResolver
        pendingLogsResolver = null
        if (resolve) resolve(parsed.reply)
      }
    }

    // Begin polling getStats() on a 500 ms cadence so the UI can show
    // live bitrate/fps/codec. Runs unconditionally while `pc` exists;
    // teardown() stops + clears it.
    startStatsPoll()

    // Resolve VP9-444 decode support before sending the request so
    // we only advertise `data-channel-vp9-444` when the browser can
    // actually decode it. The eager probe in the composable ctor
    // has likely already resolved by now, but await once more in
    // case `connect()` runs on first paint. Falling back silently
    // to webrtc when the user opted in but the browser lacks
    // support keeps the UX boring rather than broken.
    let preferredTransport: RcVideoTransport | null = null
    if (videoTransport.value === 'data-channel-vp9-444') {
      const supported = vp9_444Supported.value || (await isVp9_444DecodeSupported())
      vp9_444Supported.value = supported
      if (supported) {
        preferredTransport = 'data-channel-vp9-444'
      } else {
        console.info(
          '[rc] preferred_transport=data-channel-vp9-444 dropped — VideoDecoder.isConfigSupported(vp09.01.10.08) returned false. Falling back to webrtc.',
        )
      }
    }

    // If we're advertising the data-channel transport, open the DC +
    // worker NOW so the channel lands in the SDP offer. The agent
    // will only actually pump bytes through it when its caps include
    // the same transport, so opening it speculatively is harmless on
    // older agents (they ignore the channel entirely).
    if (preferredTransport === 'data-channel-vp9-444') {
      startVp9_444Path()
    }

    // Kick off the rc:* handshake. browser_caps lets the agent pick
    // the best codec on its end (Phase 2 commit 2B.2 wires the
    // intersection logic + SDP munging on the agent side).
    // preferred_transport (Phase Y.3) hints which transport the
    // browser would like to use; the agent honours it only if its
    // own AgentCaps.transports contains the same entry, otherwise
    // falls back to the legacy WebRTC video track silently.
    const requestPayload: Record<string, unknown> = {
      t: 'rc:session.request',
      agent_id: agentId,
      permissions,
      browser_caps: browserCaps,
    }
    if (preferredTransport) {
      requestPayload.preferred_transport = preferredTransport
    }
    ws.sendRaw(requestPayload)
  }

  function disconnect() {
    // Operator-initiated teardown must override any pending
    // reconnect timer; otherwise a reconnect could fire after the
    // user already dismissed the viewer, racing the WS rc:terminate
    // we just sent.
    cancelReconnect()
    lastConnectArgs = null
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
   * `options.onFilesPasted` is called when the operator hits Ctrl+V over
   * the viewer with files in their OS clipboard. The composable defers
   * the Ctrl+V keystroke until the `paste` event fires (a fraction of a
   * millisecond later) and decides: files → call onFilesPasted, no
   * keystroke forwarded; text → mirror to host clipboard via existing
   * `setAgentClipboard` + emit deferred Ctrl+V; empty → emit deferred
   * Ctrl+V as a fallback.
   *
   * Returns a detach function the caller should invoke before unmounting.
   */
  function attachInput(
    surface: HTMLElement,
    options?: {
      onFilesPasted?: (files: File[]) => void
      /** Element to focus on pointerenter to steal focus from left-
       *  panel nav-drawer items / page buttons. Should have
       *  `tabindex="-1"` so it doesn't enter the Tab order but
       *  accepts programmatic `.focus()`. Field bug rc.17: clicking a
       *  Dashboard / Rooms / Files nav item then connecting to the
       *  viewer left that `<v-list-item>` focused; the first Enter /
       *  Space pressed over the viewer fired Vuetify's keyboard-
       *  activation `@click` and navigated away. */
      focusAnchor?: HTMLElement
      /** Called after a Ctrl+C-over-viewer auto-mirror attempt.
       *  `ok === true`  → text written to `navigator.clipboard` OK.
       *  `ok === false` → browser refused `writeText` (no permission /
       *  no user-gesture chain); caller shows a snackbar with the
       *  text + a manual Copy button so the operator can still get
       *  the content. */
      onClipboardMirrored?: (text: string, ok: boolean) => void
    }
  ): () => void {
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
      // VP9-444 mode: the `<video>` is hidden + unfed (the agent's
      // pump routed encoded frames to the `video-bytes` DC instead of
      // the WebRTC track), so `video.videoWidth` is 0. The visible
      // surface is the `<canvas>` painted by rc-vp9-444-worker, and
      // its intrinsic dimensions (the agent's encode resolution) are
      // already cached in `mediaIntrinsicW/H` from the worker's
      // `first-frame` message. Use that instead — without it the
      // letterbox math hits divide-by-zero and every pointer event
      // gets mapped to NaN, dropping all clicks/moves silently.
      // Same shape applies to the WebCodecs render path (the canvas
      // there also reports via `first-frame`), but in that path the
      // `<video>` is also fed the RTP track so videoWidth is non-zero
      // anyway — falling through to the canvas path is harmless.
      const useCanvasDims = vp9_444Active.value || webcodecsActive.value
      const intrinsicW = useCanvasDims
        ? mediaIntrinsicW.value
        : (video?.videoWidth ?? 0)
      const intrinsicH = useCanvasDims
        ? mediaIntrinsicH.value
        : (video?.videoHeight ?? 0)
      // In `original` / `custom` scale modes the surface element is
      // sized to its own intrinsic pixels (× custom scale) — there's
      // no letterboxing inside it, so map directly against its
      // bounding rect. In `adaptive` mode the element fills the stage
      // and `object-fit: contain` letterboxes internally, so we need
      // the stage rect + aspect-ratio math.
      //
      // Pick the live render surface for the direct-bounding-rect
      // path: video in legacy mode, canvas in VP9-444 / WebCodecs
      // modes. (The `<video>` is `display: none` in the latter two,
      // so getBoundingClientRect() would report a zero rect.)
      const renderEl: HTMLElement | null = useCanvasDims
        ? (surface.querySelector('canvas.remote-video') as HTMLElement | null)
        : video
      if (scaleMode.value !== 'adaptive' && renderEl) {
        const r = renderEl.getBoundingClientRect()
        return directVideoNormalise(
          ev.clientX, ev.clientY,
          { left: r.left, top: r.top, width: r.width, height: r.height },
        )
      }
      const frameRect = surface.getBoundingClientRect()
      return letterboxedNormalise(
        ev.clientX, ev.clientY,
        { left: frameRect.left, top: frameRect.top, width: frameRect.width, height: frameRect.height },
        intrinsicW, intrinsicH,
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
    function onPointerEnter() {
      pointerInside = true
      // rc.18: steal focus from whatever the operator clicked last
      // (typically a left-panel nav-drawer item) so Enter / Space /
      // Arrow keys pressed over the viewer DON'T fire the focused
      // element's `@click` keyboard-activation handler. Two-step:
      // blur the active element + focus our anchor. Anchor has
      // tabindex="-1" so it accepts programmatic focus without
      // entering Tab order.
      if (options?.focusAnchor) {
        const active = document.activeElement
        if (active instanceof HTMLElement && active !== options.focusAnchor) {
          active.blur()
        }
        try {
          options.focusAnchor.focus({ preventScroll: true })
        } catch {
          /* old browsers without `preventScroll`: blurring above is
             enough to fix the immediate bug; the focus call is
             defence-in-depth */
        }
      } else if (document.activeElement instanceof HTMLElement) {
        // No anchor given by caller — just blur. Active element ends
        // up on <body>; harmless.
        document.activeElement.blur()
      }
    }
    function onPointerLeave() { pointerInside = false }

    // Phase 5 (file-DC v2) — deferred Ctrl+V over viewer.
    //
    // When the operator hits Ctrl+V with the pointer over the viewer,
    // we don't immediately forward the keystroke. The browser fires
    // a `paste` event microseconds later; we use that to decide:
    //   - Files in clipboard  → upload them; the remote app does
    //     NOT receive a Ctrl+V keystroke (that wasn't the operator's
    //     intent — they meant "upload these files").
    //   - Text in clipboard   → mirror to the host clipboard via
    //     the existing `clipboard:write` + emit the deferred Ctrl+V
    //     so the remote app's paste sees the right text.
    //   - Empty clipboard     → emit the deferred Ctrl+V as a normal
    //     keystroke (operator intent unclear; preserve current
    //     behaviour).
    //
    // 50 ms timeout fallback: some browsers don't fire `paste` if
    // the clipboard is empty / denied. After 50 ms with the keystroke
    // still pending, flush it as a normal Ctrl+V. 50 ms is below the
    // human keystroke-perception threshold but well above paste-event
    // scheduling.
    //
    // The keyup is also intercepted while a deferral is active so we
    // don't emit a stray V-up against an un-down'd V on the agent.
    let pendingCtrlV: { mods: number; timer: ReturnType<typeof setTimeout> | null } | null = null
    const KEY_V_HID = 0x19

    function flushPendingCtrlV() {
      if (!pendingCtrlV) return
      const mods = pendingCtrlV.mods
      if (pendingCtrlV.timer) clearTimeout(pendingCtrlV.timer)
      pendingCtrlV = null
      sendInput({ t: 'key', code: KEY_V_HID, down: true, mods })
      sendInput({ t: 'key', code: KEY_V_HID, down: false, mods })
    }

    function isCtrlVOverViewer(ev: KeyboardEvent): boolean {
      if (!pointerInside) return false
      if (ev.code !== 'KeyV') return false
      if (!(ev.ctrlKey || ev.metaKey)) return false
      // If focus is in an INPUT / TEXTAREA / contenteditable element,
      // the operator is editing a page text field — let the native
      // paste flow happen there; don't intercept.
      const target = ev.target as Element | null
      if (target) {
        const tag = target.tagName
        if (tag === 'INPUT' || tag === 'TEXTAREA') return false
        const editable = (target as HTMLElement).isContentEditable
        if (editable) return false
      }
      return true
    }

    /** Ctrl+C over viewer (rc.18 P5). Mirrors the Ctrl+V helper's
     *  carve-out for page-text-field focus so the operator's normal
     *  copy-from-form-field doesn't get hijacked. */
    function isCtrlCOverViewer(ev: KeyboardEvent): boolean {
      if (!pointerInside) return false
      if (ev.code !== 'KeyC') return false
      if (!(ev.ctrlKey || ev.metaKey)) return false
      const target = ev.target as Element | null
      if (target) {
        const tag = target.tagName
        if (tag === 'INPUT' || tag === 'TEXTAREA') return false
        const editable = (target as HTMLElement).isContentEditable
        if (editable) return false
      }
      return true
    }

    /** Schedule a 25 ms-delayed read of the host's clipboard + mirror
     *  to the browser's `navigator.clipboard`. 25 ms is enough for
     *  the remote app to finish its copy (well under human perception)
     *  and avoids a race with the agent's Ctrl+C HID handling.
     *
     *  On failure (DC closed, agent doesn't reply within 5 s, or
     *  `writeText` is refused by the browser's user-gesture policy)
     *  the caller's `onClipboardMirrored(text, false)` fires so the
     *  parent component can surface a snackbar with a manual Copy
     *  button — keeps the operator's intent reachable.
     */
    function scheduleClipboardMirror() {
      const delayMs = 25
      setTimeout(() => {
        // Fire-and-forget. We deliberately don't await in the
        // keydown handler — the host needs to process Ctrl+C before
        // its clipboard reflects the copy.
        getAgentClipboard()
          .then(async (text: string) => {
            if (!text) return // remote clipboard was empty; no-op
            let ok = false
            try {
              await navigator.clipboard.writeText(text)
              ok = true
            } catch {
              // Browser denied (no user-gesture chain, no permission,
              // or no clipboard-write API in this context). The
              // callback exposes the text for a fallback path.
              ok = false
            }
            options?.onClipboardMirrored?.(text, ok)
          })
          .catch(() => {
            // Agent didn't respond, DC closed, etc. — silent drop;
            // the operator's local clipboard stays unchanged, same
            // as pre-rc.18 behaviour.
          })
      }, delayMs)
    }

    function onKey(ev: KeyboardEvent, down: boolean) {
      // Ctrl+V deferral path. Keep preventDefault on keydown so the
      // subsequent `paste` event fires (and so the browser doesn't
      // run a default for the V key). Skip the normal sendInput path
      // — flushPendingCtrlV / paste handler will emit the keystroke
      // if the clipboard didn't have files.
      if (isCtrlVOverViewer(ev)) {
        // CRITICAL: do NOT call ev.preventDefault() on this keydown.
        // Per HTML spec, preventDefault on a keydown that would
        // trigger paste suppresses the subsequent `paste` event
        // entirely — `clipboardData` is never delivered to our
        // listener and the deferred-keystroke design degenerates
        // into "always flush as plain Ctrl+V" (rc.12-rc.15 bug).
        // Field repro rc.15 2026-05-07: Ctrl+V never uploads files.
        //
        // Instead: stash pendingCtrlV (skip the sendInput keystroke
        // forwarding) and let the browser's natural paste pipeline
        // fire. The window-level `paste` listener decides — files →
        // upload, text → clipboard:write + flush keystroke, empty
        // clipboard → 50 ms timer flushes as normal Ctrl+V.
        //
        // Keystroke forwarding is suppressed by the `return` below
        // (we exit before `sendInput`); the host won't see Ctrl+V
        // until the paste handler explicitly flushes it.
        if (down) {
          if (pendingCtrlV?.timer) clearTimeout(pendingCtrlV.timer)
          const mods =
            (ev.ctrlKey ? 1 : 0) |
            (ev.shiftKey ? 2 : 0) |
            (ev.altKey ? 4 : 0) |
            (ev.metaKey ? 8 : 0)
          const timer = setTimeout(() => {
            // Paste didn't fire (empty clipboard / browser denied
            // the read) — flush as a normal Ctrl+V keystroke so
            // the operator's chord still reaches the remote app.
            flushPendingCtrlV()
          }, 50)
          pendingCtrlV = { mods, timer }
        }
        // rc.18: stop propagation so a focused nav-drawer item
        // doesn't ALSO see the Ctrl+V and trigger its own
        // keyboard-activation. Capture-phase keydown means we run
        // before the focused element's bubble-phase handlers.
        if (pointerInside) ev.stopPropagation()
        // keyup with a pending deferral: don't emit a stray V-up.
        // The flush path emits both down + up together.
        return
      }

      const action = decideKeyAction(ev, down, (k) => ev.getModifierState(k))
      if (action.kind === 'drop') return
      if (shouldPreventDefault(ev, pointerInside)) ev.preventDefault()
      // rc.18: stop propagation when pointer is over viewer so a
      // focused page button / nav-drawer item doesn't ALSO see this
      // keystroke and fire its own keyboard-activation `@click`. The
      // capture-phase registration of this listener (see below) means
      // stopPropagation here cuts the bubble path before any focused
      // descendant runs its handler.
      if (pointerInside) ev.stopPropagation()
      if (action.kind === 'text') {
        sendInput({ t: 'key_text', text: action.text })
      } else {
        sendInput({ t: 'key', code: action.code, down: action.down, mods: action.mods })
      }
      // rc.18: after a Ctrl+C-over-viewer is forwarded to the host,
      // schedule the auto-mirror of the host's clipboard back to the
      // browser. Only fire on `down` (Ctrl+C produces a down event;
      // we don't want to fire twice on up). Carve-outs (focus inside
      // INPUT/TEXTAREA/contenteditable) live in isCtrlCOverViewer.
      if (down && isCtrlCOverViewer(ev)) {
        scheduleClipboardMirror()
      }
    }

    function onPaste(ev: ClipboardEvent) {
      // Only respond if we deferred a Ctrl+V keystroke. Native paste
      // events that come from elsewhere (e.g. an editable field
      // outside the deferral path) keep their default handling.
      if (!pendingCtrlV) return
      const dt = ev.clipboardData
      if (!dt) {
        flushPendingCtrlV()
        return
      }

      // Files take precedence — operator intent is "upload these".
      if (dt.files && dt.files.length > 0 && options?.onFilesPasted) {
        ev.preventDefault()
        if (pendingCtrlV.timer) clearTimeout(pendingCtrlV.timer)
        pendingCtrlV = null
        const files: File[] = []
        for (let i = 0; i < dt.files.length; i++) files.push(dt.files[i])
        options.onFilesPasted(files)
        return
      }

      // Text path: mirror to host clipboard so the remote app's
      // paste sees the right content, then emit the Ctrl+V keystroke.
      const text = dt.getData('text') ?? ''
      if (text) {
        const ch = channels.clipboard
        if (ch && ch.readyState === 'open') {
          try {
            ch.send(JSON.stringify({ t: 'clipboard:write', text }))
          } catch {
            /* dropped — host clipboard stays unchanged but we still
               forward the keystroke; remote app pastes whatever was
               there before. */
          }
        }
      }
      flushPendingCtrlV()
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
    // Paste handler must be on `window` (or a focusable surface) —
    // attaching to `surface` only fires when surface itself is the
    // event target, which doesn't happen for keyboard-driven paste.
    // Window-level listener with our own pendingCtrlV gating means
    // we only intercept paste events that follow a deferred Ctrl+V.
    window.addEventListener('paste', onPaste)
    // rc.18: register on CAPTURE phase so we run BEFORE any focused
    // element's bubble-phase handlers. Combined with the per-handler
    // `stopPropagation` when pointer is inside, this stops a focused
    // nav-drawer item from receiving Enter/Space/etc. while the
    // operator is driving the remote. Outside the viewer the
    // stopPropagation is gated off, so normal browser shortcuts
    // (Tab navigation, Esc closing dialogs) still work.
    window.addEventListener('keydown', onKeyDown, { capture: true })
    window.addEventListener('keyup', onKeyUp, { capture: true })

    return () => {
      surface.removeEventListener('pointermove', onPointerMove)
      surface.removeEventListener('pointerdown', onPointerDown)
      surface.removeEventListener('pointerup', onPointerUp)
      surface.removeEventListener('pointerenter', onPointerEnter)
      surface.removeEventListener('pointerleave', onPointerLeave)
      surface.removeEventListener('wheel', onWheel)
      surface.removeEventListener('contextmenu', onContextMenu)
      window.removeEventListener('paste', onPaste)
      // Same `capture: true` as the add — required for matching.
      window.removeEventListener('keydown', onKeyDown, { capture: true })
      window.removeEventListener('keyup', onKeyUp, { capture: true })
      // Drop any in-flight deferral; otherwise its 50 ms timer would
      // fire after teardown and call sendInput on a closed channel.
      if (pendingCtrlV?.timer) clearTimeout(pendingCtrlV.timer)
      pendingCtrlV = null
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

  /** Upload a single `File` to the remote host's Downloads folder via
   *  the `files` data channel. Chunks at 64 KiB with backpressure on
   *  the SCTP buffer. Resolves with the final path + byte count
   *  reported by the agent. Rejects on agent error or DC close.
   *
   *  Internal — the public surface is `uploadFiles(files)` (queue) and
   *  the back-compat `uploadFile(file)` shim. Reads agent replies via
   *  the persistent `files` DC listener registered at channel-create
   *  time (see `filesRegistry`). */
  /**
   * Sentinel "DC closed mid-upload" error. The resume wrapper
   * catches THIS specifically and retries; any other Error
   * propagates straight to the caller.
   */
  const CHANNEL_CLOSED_TAG = '__rc19_channel_closed__'
  function makeChannelClosedError(file: File, offset: number): Error {
    const pct = file.size === 0 ? 100 : Math.round((offset / file.size) * 100)
    const err = new Error(
      `files channel closed mid-upload at ${pct}% (${offset}/${file.size} bytes). ` +
        `Most likely the remote agent restarted (auto-update / crash / network drop).`
    )
    ;(err as Error & { [k: string]: unknown })[CHANNEL_CLOSED_TAG] = true
    return err
  }
  function isChannelClosedError(e: unknown): boolean {
    return !!(e && typeof e === 'object' && (e as Record<string, unknown>)[CHANNEL_CLOSED_TAG])
  }

  /**
   * Inner pump — sends raw chunks from `file.slice(startOffset)` over
   * the LIVE `channels.files` DC (re-read every invocation, NOT
   * captured at construction). Throws a channel-closed sentinel when
   * the DC dies; caller handles retry via `uploadOneResumable`.
   * Sends the terminal `files:end` envelope on success.
   *
   * Used by both the first attempt (after `files:begin`) and every
   * resume attempt (after `files:resumed`).
   */
  async function innerPump(file: File, startOffset: number, id: string): Promise<void> {
    const CHUNK = 64 * 1024
    let offset = startOffset
    // Cancellation: `cancelUpload(id)` settles the registry entry
    // locally; the pump checks status between chunks and exits.
    const isCancelled = () => {
      const e = filesRegistry.get(id)
      return !e || e.status === 'settled'
    }
    while (offset < file.size) {
      if (isCancelled()) return
      // P0-3 fix: re-read live channel every loop iteration. After a
      // DC drop + reconnect, `channels.files` is a NEW
      // RTCDataChannel; the stale capture would throw "DataChannel
      // is not opened" indefinitely.
      const ch = channels.files
      if (!ch || ch.readyState !== 'open') {
        throw makeChannelClosedError(file, offset)
      }
      // Back off when the sctp buffer fills up so the browser
      // doesn't OOM on huge files.
      while (ch.bufferedAmount > 4 * 1024 * 1024) {
        if (ch.readyState !== 'open') {
          throw makeChannelClosedError(file, offset)
        }
        if (isCancelled()) return
        await new Promise((r) => setTimeout(r, 20))
      }
      const end = Math.min(offset + CHUNK, file.size)
      const slice = file.slice(offset, end)
      const buf = await slice.arrayBuffer()
      // Re-read after the await — readyState can flip during the
      // ArrayBuffer materialisation.
      const ch2 = channels.files
      if (!ch2 || ch2.readyState !== 'open') {
        throw makeChannelClosedError(file, offset)
      }
      if (isCancelled()) return
      try {
        ch2.send(buf)
      } catch {
        throw makeChannelClosedError(file, offset)
      }
      offset = end
      patchTransfer(id, { status: 'running', bytes: offset })
    }
    if (isCancelled()) return
    const ch = channels.files
    if (!ch || ch.readyState !== 'open') {
      throw makeChannelClosedError(file, offset)
    }
    try {
      ch.send(JSON.stringify({ t: 'files:end', id }))
    } catch {
      throw makeChannelClosedError(file, offset)
    }
  }

  /**
   * Wait until the WebRTC peer is back in `connected` phase OR the
   * reconnect ladder gives up (phase transitions to 'error' or
   * 'closed'). Resolves true on connected, false on terminal.
   * Cancelled by `onBeforeUnmount` via the same `stop` registry as
   * the rest of the composable.
   */
  function waitForConnected(timeoutMs: number = 30_000): Promise<boolean> {
    if (phase.value === 'connected') return Promise.resolve(true)
    // rc.23 — pass `Number.POSITIVE_INFINITY` to wait indefinitely
    // for the next 'connected' transition. `setTimeout(fn, Infinity)`
    // is implementation-defined (most engines clamp to ~2^31-1 ms
    // ≈ 25 days) so we just skip the timer instead. The settle path
    // is the phase watcher below — phases 'closed' / 'error' / 'idle'
    // still resolve false so the caller can detect operator-cancel.
    return new Promise((resolve) => {
      const wantTimer = Number.isFinite(timeoutMs)
      const timer = wantTimer
        ? setTimeout(() => {
            stop()
            resolve(false)
          }, timeoutMs)
        : null
      const stop = watch(
        phase,
        (p) => {
          if (p === 'connected') {
            if (timer !== null) clearTimeout(timer)
            stop()
            resolve(true)
          } else if (p === 'closed' || p === 'error' || p === 'idle') {
            if (timer !== null) clearTimeout(timer)
            stop()
            resolve(false)
          }
        },
        { immediate: false }
      )
    })
  }

  /**
   * rc.23 hotfix — wait for `channels.files` to be open. Necessary
   * companion to {@link waitForConnected} for the resume loop:
   * `phase = 'connected'` only means the WebRTC PeerConnection is
   * up, not that the file DC has re-opened. When the agent drops
   * the file DC mid-transfer but keeps the peer alive (some failure
   * modes do this), `runOnce` throws "channel closed" synchronously,
   * `waitForConnected` returns true immediately, the loop re-enters,
   * throws again — tight async loop that burns CPU and freezes the
   * tab. Field repro on PC50045 2026-05-12 (CV.pdf upload + rc.23
   * web on rc.23 agent → tab had to be killed).
   *
   * `pollIntervalMs` defaults to 250 — cheap; the DC opens once per
   * resume cycle so we don't pay a steady cost. `timeoutMs` defaults
   * to `Number.POSITIVE_INFINITY` so the loop matches the parent
   * "DC always stays open" contract; finite caps are useful only
   * when an outer caller wants to bail.
   */
  function waitForFilesChannel(
    timeoutMs: number = Number.POSITIVE_INFINITY,
    pollIntervalMs: number = 250
  ): Promise<boolean> {
    if (channels.files && channels.files.readyState === 'open') {
      return Promise.resolve(true)
    }
    return new Promise((resolve) => {
      const wantTimer = Number.isFinite(timeoutMs)
      let settled = false
      const finish = (ok: boolean) => {
        if (settled) return
        settled = true
        if (timer !== null) clearTimeout(timer)
        clearInterval(poll)
        resolve(ok)
      }
      const timer = wantTimer ? setTimeout(() => finish(false), timeoutMs) : null
      const poll = setInterval(() => {
        if (channels.files && channels.files.readyState === 'open') {
          finish(true)
          return
        }
        // Operator-disconnect / fatal error terminates the wait.
        if (phase.value === 'closed' || phase.value === 'error' || phase.value === 'idle') {
          finish(false)
        }
      }, pollIntervalMs)
    })
  }

  /**
   * rc.19 P5: resume-capable wrapper around `innerPump`. First
   * attempt sends `files:begin`; subsequent attempts send
   * `files:resume { id, offset: entry.bytesAcked }` and re-pump
   * from the agent's accepted offset. Up to 6 attempts (matches
   * RC_RECONNECT_LADDER_MS.length); on exhaustion sends
   * `files:cancel` so the agent cleans its staging dir immediately.
   *
   * Pre-rc.19 behaviour preserved for non-resume agents: the
   * `supportsResume.value === false` branch falls through to a
   * single fresh-begin attempt with the original fail-fast error.
   */
  function uploadOne(
    file: File,
    relPath?: string,
    destPath?: string
  ): Promise<{ path: string; bytes: number }> {
    const initialCh = channels.files
    if (!initialCh || initialCh.readyState !== 'open') {
      return Promise.reject(new Error('files channel not open'))
    }
    const id = `up-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`

    return new Promise((resolve, reject) => {
      const entry: UploadEntry = {
        kind: 'upload',
        status: 'pending',
        resolve,
        reject,
        bytesAcked: 0,
        file,
        relPath,
        destPath,
      }
      filesRegistry.set(id, entry)
      pushTransfer({
        id,
        kind: 'upload',
        // Show the relative path in the Transfers panel for folder
        // uploads so the operator can tell `file.txt` (root) apart
        // from `MyFolder/sub/file.txt` (deep).
        name: relPath ?? file.name,
        bytes: 0,
        total: file.size,
        status: 'queued',
      })

      // Local error → settle the registry entry ourselves and reject.
      // (Agent-side errors arrive via the persistent listener.)
      const localFail = (err: Error) => {
        const settled = settleEntry(id)
        if (settled) {
          patchTransfer(id, { status: 'error', error: err.message })
          settled.reject(err)
        }
      }

      function sendBegin(ch: RTCDataChannel): boolean {
        try {
          if (ch.readyState !== 'open') {
            throw new Error('files channel closed before files:begin could be sent')
          }
          // Folder-upload extension (file-DC v2.1) + path-targeted
          // upload extension (v2.2). Old agents ignore unknown JSON
          // fields and use `name` as the basename.
          const beginMsg: Record<string, unknown> = {
            t: 'files:begin',
            id,
            name: file.name,
            size: file.size,
            mime: file.type || undefined,
          }
          if (relPath) beginMsg.rel_path = relPath
          if (destPath) beginMsg.dest_path = destPath
          ch.send(JSON.stringify(beginMsg))
          return true
        } catch (e) {
          localFail(e instanceof Error ? e : new Error(String(e)))
          return false
        }
      }

      // rc.19: send `files:resume { id, offset }` and await the
      // matching `files:resumed { id, accepted_offset }` (or
      // `files:error` → reject the waiter, wrapper falls back to a
      // fresh `files:begin` with a NEW id). 10 s timeout — way more
      // than the agent's local lookup + truncate + reopen.
      function sendResume(ch: RTCDataChannel, offset: number): Promise<number> {
        return new Promise<number>((resolveResume, rejectResume) => {
          const timer = setTimeout(() => {
            pendingResumePromises.delete(id)
            rejectResume(new Error('files:resumed timeout'))
          }, 10_000)
          pendingResumePromises.set(id, {
            resolve: resolveResume,
            reject: rejectResume,
            timer,
          })
          try {
            if (ch.readyState !== 'open') {
              throw new Error('files channel closed before files:resume could be sent')
            }
            ch.send(JSON.stringify({ t: 'files:resume', id, offset }))
          } catch (e) {
            clearTimeout(timer)
            pendingResumePromises.delete(id)
            rejectResume(e instanceof Error ? e : new Error(String(e)))
          }
        })
      }

      function sendCancelBestEffort(): void {
        const ch = channels.files
        if (!ch || ch.readyState !== 'open') return
        try {
          ch.send(JSON.stringify({ t: 'files:cancel', id }))
        } catch {
          /* DC closed between check and send — agent's 24h sweep cleans the partial */
        }
      }

      // rc.23 — infinite retry. The DC effectively stays "open" from
      // the operator's POV: every drop triggers a resume on the next
      // 'connected' transition; the loop only exits when the operator
      // cancels via Cancel button (which settles the registry entry)
      // or files:complete arrives. `MAX_ATTEMPTS` retained as a label
      // for log lines + UI "attempt N/MAX" rendering, but tied to
      // `Number.POSITIVE_INFINITY` so the budget check below is a
      // no-op. Was 6; rolled forward on PC50045 field repro where
      // ESET caused the agent to be killed repeatedly during 14 MB
      // uploads and the 6-attempt cap surfaced "exhausted" before
      // the DC could reconnect.
      const MAX_ATTEMPTS = Number.POSITIVE_INFINITY
      let attempt = 0

      const runOnce = async (): Promise<void> => {
        // Bail if the operator cancelled or a fatal error already
        // settled the registry entry.
        const e = filesRegistry.get(id)
        if (!e || e.status === 'settled') return

        let startOffset = 0
        const ch = channels.files
        if (!ch || ch.readyState !== 'open') {
          throw makeChannelClosedError(file, e.kind === 'upload' ? e.bytesAcked : 0)
        }

        if (attempt === 0) {
          // First attempt — fresh begin.
          if (!sendBegin(ch)) return
          // Transition the entry back to 'pending' (it might have been
          // 'pending-resume' if this is a fresh-id fallback after a
          // resume error).
          if (e.kind === 'upload') e.status = 'pending'
          patchTransfer(id, { status: 'running' })
        } else {
          // Resume attempt. supportsResume is already guaranteed
          // true by the catch-block guard below.
          patchTransfer(id, {
            status: 'reconnecting',
            error: `attempt ${attempt + 1}`,
          })
          // Transition back to 'pending' so files:complete settles
          // through the normal path.
          if (e.kind === 'upload') e.status = 'pending'
          const requested = e.kind === 'upload' ? e.bytesAcked : 0
          const accepted = await sendResume(ch, requested)
          startOffset = accepted
          if (e.kind === 'upload') e.bytesAcked = accepted
          patchTransfer(id, { status: 'running', bytes: startOffset })
        }
        await innerPump(file, startOffset, id)
        // files:end is sent by innerPump on success; the listener
        // resolves the outer promise on files:complete.
      }

      const runResumable = async (): Promise<void> => {
        while (attempt < MAX_ATTEMPTS) {
          try {
            await runOnce()
            return // success — listener resolves via files:complete
          } catch (err) {
            const e = filesRegistry.get(id)
            if (!e || e.status === 'settled') return // settled by listener
            const canRetry = supportsResume.value && isChannelClosedError(err)
            if (!canRetry) {
              localFail(err instanceof Error ? err : new Error(String(err)))
              return
            }
            // Channel closed mid-flight with resume cap — wait for
            // reconnect and retry.
            patchTransfer(id, {
              status: 'reconnecting',
              error: `attempt ${attempt + 1}`,
            })
            // rc.23 — wait forever for the peer to come back. The
            // resume loop is the operator's "DC stays open" promise;
            // legacy 30 s timeout could fire while the agent was
            // being installer-restarted by msiexec (5-90 s window
            // observed during auto-update on PC50045). Outer
            // settle-check at the top of `runOnce` handles the
            // operator-cancel path.
            const e2 = filesRegistry.get(id)
            if (!e2 || e2.status === 'settled') return
            // rc.23 hotfix — bound the retry rate to prevent a tight
            // async loop when the file DC is closed but the peer is
            // still 'connected'. Without this delay, `runOnce` throws
            // "channel closed" synchronously, `waitForConnected`
            // returns true immediately (phase already 'connected'),
            // we retry, throw again — thousands of iterations per
            // second pin a CPU core and freeze the browser tab. Field
            // repro on PC50045 2026-05-12 (CV.pdf upload). Backstop
            // delay also gives the agent breathing room to reopen
            // the DC before we ping it again.
            const backoffMs =
              attempt < RC_RECONNECT_LADDER_MS.length
                ? RC_RECONNECT_LADDER_MS[attempt]
                : RC_RECONNECT_STEADY_MS
            await new Promise((r) => setTimeout(r, backoffMs))
            // Re-check settled after the sleep — operator may have
            // cancelled while we were waiting.
            const e3 = filesRegistry.get(id)
            if (!e3 || e3.status === 'settled') return
            // Block until 'connected' fires; the watch handler that
            // resolves waitForConnected fires unconditionally on the
            // peer-level 'connected' transition.
            const connected = await waitForConnected(Number.POSITIVE_INFINITY)
            if (!connected) {
              // waitForConnected only returns false on timeout. With
              // an infinite timeout the only way out is the settle
              // check above; surface a defensive error if we somehow
              // get here.
              localFail(new Error('reconnect wait returned without connecting (defensive)'))
              return
            }
            // rc.23 hotfix — also wait for the file DC to re-open.
            // `phase === 'connected'` is necessary but not sufficient:
            // if the agent dropped just the file DC, the peer never
            // transitions, and `runOnce` would throw on entry without
            // the DC ready. waitForFilesChannel polls every 250 ms
            // for `channels.files.readyState === 'open'`.
            const fileChanOpen = await waitForFilesChannel(Number.POSITIVE_INFINITY)
            if (!fileChanOpen) {
              localFail(new Error('file channel did not re-open (operator disconnect?)'))
              return
            }
            attempt += 1
          }
        }
        // MAX_ATTEMPTS is Infinity in rc.23 — this is unreachable but
        // kept as a defensive surface so a future regression flips
        // MAX_ATTEMPTS without leaving the loop able to silently exit.
        sendCancelBestEffort()
        localFail(new Error('resumable upload exited the infinite-retry loop unexpectedly'))
      }

      void runResumable()
    })
  }

  /** Public single-file upload — back-compat shim retained so existing
   *  E2E tests + 0.2.x call sites keep working. New code should call
   *  `uploadFiles([file])` directly. */
  function uploadFile(file: File): Promise<{ path: string; bytes: number }> {
    return uploadOne(file)
  }

  // --------------------------------------------------------------
  // Downloads (host → browser) — Phase 2 of file-DC v2.

  // Hard cap on the Blob fallback path (no showSaveFilePicker) so a
  // misbehaving server can't OOM the tab by streaming gigabytes of
  // chunks into memory. Single-file downloads come with size up-front
  // (in files:offer); we can refuse early. Folder zips (Phase 4) ride
  // a Chrome-only path that bypasses this entirely.
  const DOWNLOAD_BLOB_HARD_CAP = 2 * 1024 * 1024 * 1024 // 2 GiB

  /** Append one binary chunk to a download entry's sink. Tracks total
   *  bytes for the Transfers panel + enforces the Blob-fallback hard
   *  cap so a wedged stream doesn't OOM the tab. */
  function appendDownloadChunk(entry: DownloadEntry, buf: ArrayBuffer) {
    if (entry.status === 'settled') return
    entry.bytesReceived += buf.byteLength
    patchTransfer(entry === filesRegistry.get(activeDownloadId ?? '') ? (activeDownloadId as string) : '', {
      status: 'running',
      bytes: entry.bytesReceived,
    })
    if (entry.saveMode === 'stream' && entry.writable) {
      // Chrome streaming path: write directly to the user-chosen
      // file. Errors propagate via files:error from the agent or
      // via the writable's own promise (handled in finalizeDownload).
      void entry.writable.write(buf).catch(() => {
        // Swallow; the writable's close()/abort() in finalize will
        // surface the underlying error.
      })
    } else if (entry.saveMode === 'blob') {
      if (entry.bytesReceived > DOWNLOAD_BLOB_HARD_CAP) {
        // Sentinel: settle now with an error and drop the buffer.
        // Send a cancel to the agent so it stops sending bytes.
        const id = activeDownloadId
        if (id) {
          const ch = channels.files
          if (ch && ch.readyState === 'open') {
            try { ch.send(JSON.stringify({ t: 'files:cancel', id })) } catch { /* dropped */ }
          }
          const settled = settleEntry(id)
          if (settled?.kind === 'download') {
            const msg = `download exceeds ${Math.round(DOWNLOAD_BLOB_HARD_CAP / (1024 * 1024 * 1024))} GiB browser-memory cap (use Chrome for streaming downloads)`
            patchTransfer(id, { status: 'error', error: msg })
            settled.reject(new Error(msg))
          }
          activeDownloadId = null
        }
        return
      }
      entry.blobs.push(buf)
    }
  }

  /** Close the writable (Chrome streaming) OR concatenate the Blob
   *  parts and trigger an `<a download>` click (Firefox/Safari
   *  fallback). Resolves once the bytes are durable. */
  async function finalizeDownload(entry: DownloadEntry, totalBytes: number): Promise<void> {
    if (entry.saveMode === 'stream' && entry.writable) {
      await entry.writable.close()
      return
    }
    if (entry.saveMode === 'blob') {
      const blob = new Blob(entry.blobs, { type: entry.mime || 'application/octet-stream' })
      const url = URL.createObjectURL(blob)
      try {
        const a = document.createElement('a')
        a.href = url
        a.download = entry.suggestedName || entry.name || 'download.bin'
        a.style.display = 'none'
        document.body.appendChild(a)
        a.click()
        document.body.removeChild(a)
      } finally {
        // Schedule the URL revoke after the browser has had a tick
        // to start the download.
        setTimeout(() => URL.revokeObjectURL(url), 1_000)
      }
      // Free the chunks regardless of success.
      entry.blobs = []
      // Sanity: don't surface a totalBytes mismatch as an error here;
      // the agent already locked the count via files:eof.
      void totalBytes
      return
    }
    // 'pending' should never reach here (files:offer flips it to
    // 'blob' as a fallback); guard anyway.
    throw new Error('download finalised in unexpected save mode')
  }

  /** Download a single file from the host to the browser's local
   *  filesystem. `path` is the absolute host path (subject to the
   *  agent's denylist). `suggestedName` overrides the filename in the
   *  save dialog / anchor download. Returns the agent-reported byte
   *  count on success.
   *
   *  Implementation strategy:
   *  - Chrome / Edge / Safari 17+: opens `showSaveFilePicker` BEFORE
   *    sending the request (browsers require a user-gesture chain),
   *    streams chunks directly into the chosen file.
   *  - Firefox / Safari < 17: accumulates chunks in an in-memory
   *    Blob and triggers an `<a download>` click on completion.
   *    Capped at 2 GiB to prevent OOM.
   */
  async function downloadFile(
    path: string,
    suggestedName?: string
  ): Promise<{ name: string; bytes: number }> {
    const ch = channels.files
    if (!ch || ch.readyState !== 'open') {
      throw new Error('files channel not open')
    }
    const id = `dl-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`
    const fallbackName = suggestedName ?? path.split(/[\\/]/).pop() ?? 'download.bin'

    // Try to open showSaveFilePicker FIRST (before any await past the
    // user gesture) — browsers require this. If unavailable, fall
    // back to the Blob path; if the user cancels the picker, throw.
    let writable: SaveWritable | null = null
    let saveMode: DownloadEntry['saveMode'] = 'pending'
    type ShowSaveFilePicker = (options?: {
      suggestedName?: string
      types?: { description?: string; accept: Record<string, string[]> }[]
    }) => Promise<{
      createWritable: () => Promise<SaveWritable>
    }>
    const showSavePicker = (window as unknown as { showSaveFilePicker?: ShowSaveFilePicker })
      .showSaveFilePicker
    if (typeof showSavePicker === 'function') {
      try {
        const handle = await showSavePicker({ suggestedName: fallbackName })
        writable = await handle.createWritable()
        saveMode = 'stream'
      } catch (e) {
        // User cancelled or picker errored — propagate as user-facing
        // error so the UI can show "Download cancelled".
        throw e instanceof Error ? e : new Error(String(e))
      }
    } else {
      saveMode = 'blob'
    }

    return new Promise<{ name: string; bytes: number }>((resolve, reject) => {
      const entry: DownloadEntry = {
        kind: 'download',
        status: 'pending',
        resolve,
        reject,
        saveMode,
        writable,
        blobs: [],
        name: fallbackName,
        suggestedName: fallbackName,
        bytesReceived: 0,
        expectedSize: null,
      }
      filesRegistry.set(id, entry)
      pushTransfer({
        id,
        kind: 'download',
        name: fallbackName,
        bytes: 0,
        total: null,
        status: 'queued',
      })
      try {
        ch.send(JSON.stringify({ t: 'files:get', id, path }))
      } catch (e) {
        const settled = settleEntry(id)
        if (settled?.kind === 'download') {
          const msg = e instanceof Error ? e.message : String(e)
          patchTransfer(id, { status: 'error', error: msg })
          if (writable) void writable.abort(msg).catch(() => {})
          settled.reject(new Error(msg))
        }
      }
    })
  }

  /** Download an entire folder from the host as a streaming zip.
   *  Same `files:offer` → binary chunks → `files:eof` envelope as
   *  `downloadFile`, but the agent zips on the fly with no temp
   *  disk and `size` arrives as `null` (unknown until end-of-stream).
   *
   *  **Refused on browsers without `showSaveFilePicker`** (Firefox,
   *  Safari < 17, older mobile). Folder zips don't have an upfront
   *  size, so the Blob fallback would risk OOMing on a large folder.
   *  Operators on those browsers see a clear toast asking them to
   *  use Chrome/Edge; download-individual-files still works. */
  async function downloadFolder(
    path: string,
    suggestedName?: string
  ): Promise<{ name: string; bytes: number }> {
    const ch = channels.files
    if (!ch || ch.readyState !== 'open') {
      throw new Error('files channel not open')
    }
    type ShowSaveFilePicker = (options?: {
      suggestedName?: string
      types?: { description?: string; accept: Record<string, string[]> }[]
    }) => Promise<{
      createWritable: () => Promise<SaveWritable>
    }>
    const showSavePicker = (window as unknown as { showSaveFilePicker?: ShowSaveFilePicker })
      .showSaveFilePicker
    if (typeof showSavePicker !== 'function') {
      throw new Error(
        'Folder downloads require Chrome / Edge (need streaming disk writes — Firefox / Safari fallback would OOM on large zips)'
      )
    }
    const id = `dlf-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`
    const folderBase = path.split(/[\\/]/).filter(Boolean).pop() ?? 'folder'
    const fallbackName = suggestedName ?? `${folderBase}.zip`

    let writable: SaveWritable | null = null
    try {
      const handle = await showSavePicker({
        suggestedName: fallbackName,
        types: [
          {
            description: 'ZIP archive',
            accept: { 'application/zip': ['.zip'] },
          },
        ],
      })
      writable = await handle.createWritable()
    } catch (e) {
      throw e instanceof Error ? e : new Error(String(e))
    }

    return new Promise<{ name: string; bytes: number }>((resolve, reject) => {
      const entry: DownloadEntry = {
        kind: 'download',
        status: 'pending',
        resolve,
        reject,
        saveMode: 'stream',
        writable,
        blobs: [],
        name: fallbackName,
        suggestedName: fallbackName,
        bytesReceived: 0,
        expectedSize: null,
      }
      filesRegistry.set(id, entry)
      pushTransfer({
        id,
        kind: 'download',
        name: fallbackName,
        bytes: 0,
        total: null,
        status: 'queued',
      })
      try {
        ch.send(JSON.stringify({ t: 'files:get-folder', id, path, format: 'zip' }))
      } catch (e) {
        const settled = settleEntry(id)
        if (settled?.kind === 'download') {
          const msg = e instanceof Error ? e.message : String(e)
          patchTransfer(id, { status: 'error', error: msg })
          if (writable) void writable.abort(msg).catch(() => {})
          settled.reject(new Error(msg))
        }
      }
    })
  }

  /** Ask the agent to cancel an in-flight download. Best-effort: the
   *  agent flips its AtomicBool and the next chunk-loop iteration
   *  exits. The Promise rejects via the resulting `files:error`. */
  function cancelDownload(id: string): void {
    const ch = channels.files
    if (!ch || ch.readyState !== 'open') return
    try {
      ch.send(JSON.stringify({ t: 'files:cancel', id }))
    } catch {
      /* dropped */
    }
  }

  /** Cancel an in-flight upload. Settles the registry entry locally
   *  (rejects the Promise + flags the Transfer panel row as
   *  cancelled). The browser-side `pump()` loop checks `readyState`
   *  + the entry status before each chunk send, so by settling here
   *  the next iteration short-circuits and stops sending bytes. The
   *  agent will see the DC stay open with no more chunks; eventually
   *  its existing short-transfer-on-end logic / DC-close cleanup
   *  handles the half-uploaded file (left on disk under Downloads/
   *  for the operator to delete or resume manually).
   *
   *  Symmetric with `cancelDownload` so the Transfers panel can
   *  render a single "Cancel" affordance regardless of direction. */
  function cancelUpload(id: string): void {
    const entry = filesRegistry.get(id)
    if (!entry || entry.kind !== 'upload' || entry.status === 'settled') return
    const settled = settleEntry(id)
    if (settled?.kind === 'upload') {
      patchTransfer(id, { status: 'cancelled', error: 'cancelled by operator' })
      settled.reject(new Error('cancelled by operator'))
    }
  }

  /** Cancel a transfer regardless of direction. Convenience for the
   *  Transfers panel UI which doesn't need to know upload vs
   *  download — it just calls `cancelTransfer(id)` per row. */
  function cancelTransfer(id: string): void {
    const entry = filesRegistry.get(id)
    if (!entry) return
    if (entry.kind === 'upload') cancelUpload(id)
    else cancelDownload(id)
  }

  // --------------------------------------------------------------
  // Directory listing (Phase 3 of file-DC v2).
  //
  // Request/response keyed by req_id, like the clipboard:read flow.
  // 5 s timeout rejects stale requests so the drawer doesn't spin
  // forever if the host is unreachable or the agent lacks the new
  // capability (old 0.2.x agents will simply not reply).

  type DirEntry = {
    name: string
    is_dir: boolean
    size: number | null
    mtime_unix: number | null
  }
  type DirListing = {
    path: string
    parent: string | null
    entries: DirEntry[]
  }
  const pendingDirRequests = new Map<
    string,
    { resolve: (l: DirListing) => void; reject: (e: Error) => void; timer: ReturnType<typeof setTimeout> }
  >()
  function settleDirRequest(reqId: string): {
    resolve: (l: DirListing) => void
    reject: (e: Error) => void
  } | null {
    const p = pendingDirRequests.get(reqId)
    if (!p) return null
    clearTimeout(p.timer)
    pendingDirRequests.delete(reqId)
    return { resolve: p.resolve, reject: p.reject }
  }

  /** List a directory on the host. `path` is the absolute host path;
   *  empty string / "~" / "/" enumerates roots (logical drives on
   *  Windows; "/" on Unix). Resolves with the listing or rejects on
   *  timeout / dir-error. */
  function listDir(path: string): Promise<DirListing> {
    const ch = channels.files
    if (!ch || ch.readyState !== 'open') {
      return Promise.reject(new Error('files channel not open'))
    }
    const reqId = `ls-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`
    return new Promise<DirListing>((resolve, reject) => {
      const timer = setTimeout(() => {
        if (pendingDirRequests.has(reqId)) {
          pendingDirRequests.delete(reqId)
          reject(new Error('list_dir timed out (5 s) — host may not support remote browse'))
        }
      }, 5_000)
      pendingDirRequests.set(reqId, { resolve, reject, timer })
      try {
        ch.send(JSON.stringify({ t: 'files:dir', req_id: reqId, path }))
      } catch (e) {
        clearTimeout(timer)
        pendingDirRequests.delete(reqId)
        reject(e instanceof Error ? e : new Error(String(e)))
      }
    })
  }

  /** Upload multiple files sequentially. Each file is queued through
   *  `uploadOne`; the queue continues on individual failures so a
   *  bad file doesn't sink the rest. Resolves with one result per
   *  input file (in order) carrying either the agent-reported path +
   *  bytes (success) or an error message (failure).
   *
   *  Accepts either bare `File` items (flat upload — file lands in
   *  Downloads/) or `{ file, relPath }` pairs (folder upload — agent
   *  recreates the directory structure under Downloads/<root>/).
   *  Mixing is allowed — useful when a drag&drop event has both
   *  individual files and one or more folders. */
  type UploadResult =
    | { ok: true; name: string; path: string; bytes: number }
    | { ok: false; name: string; error: string }
  // `relPath` carries the folder-upload structure (file-DC v2.1).
  // `destPath` is the path-targeted upload root (file-DC v2.2) — when
  // set, the file lands under `<destPath>/`. The two stack: a folder
  // dropped onto a host directory recreates the source structure
  // under that target dir.
  type UploadInput =
    | File
    | { file: File; relPath?: string; destPath?: string }
  async function uploadFiles(
    items: UploadInput[],
    options?: { destPath?: string }
  ): Promise<UploadResult[]> {
    const results: UploadResult[] = []
    for (const it of items) {
      const f: File = it instanceof File ? it : it.file
      const relPath: string | undefined = it instanceof File ? undefined : it.relPath
      // Per-item destPath wins; falls back to the call-level option.
      const destPath: string | undefined =
        (it instanceof File ? undefined : it.destPath) ?? options?.destPath
      const reportName = relPath ?? f.name
      try {
        const r = await uploadOne(f, relPath, destPath)
        results.push({ ok: true, name: reportName, path: r.path, bytes: r.bytes })
      } catch (e) {
        results.push({
          ok: false,
          name: reportName,
          error: e instanceof Error ? e.message : String(e),
        })
      }
    }
    return results
  }

  /** Recursively walk a `FileSystemEntry` (from
   *  `dataTransfer.items[i].webkitGetAsEntry()`) into a flat list of
   *  `{ file, relPath }` pairs ready for `uploadFiles`. The relative
   *  path uses forward slashes (matches what the agent expects on
   *  the wire). Skips dotfiles and symlinks for safety; caps the
   *  walk at 5000 files / 32 levels of depth to refuse pathological
   *  inputs (huge `node_modules` etc.) before they swamp the queue.
   *
   *  Returns `null` on entries that aren't directories (caller
   *  should treat as a single file via `entry.file()`). */
  type FolderWalkEntry = { file: File; relPath: string }
  async function walkFolderEntry(
    entry: FileSystemEntry,
    rootName: string
  ): Promise<FolderWalkEntry[]> {
    const out: FolderWalkEntry[] = []
    const MAX_FILES = 5000
    const MAX_DEPTH = 32
    type Pending = { entry: FileSystemEntry; relParent: string; depth: number }
    const queue: Pending[] = [{ entry, relParent: rootName, depth: 0 }]
    while (queue.length > 0) {
      const { entry: cur, relParent, depth } = queue.shift() as Pending
      if (depth > MAX_DEPTH) continue
      if (cur.name.startsWith('.')) continue // skip dotfiles / dotdirs
      if (cur.isFile) {
        const fileEntry = cur as FileSystemFileEntry
        const f: File = await new Promise((resolve, reject) =>
          fileEntry.file(resolve, reject)
        )
        out.push({ file: f, relPath: `${relParent}/${cur.name}` })
        if (out.length >= MAX_FILES) break
      } else if (cur.isDirectory) {
        const dirEntry = cur as FileSystemDirectoryEntry
        const reader = dirEntry.createReader()
        // readEntries pages — keep calling until it returns an empty array.
        let batch: FileSystemEntry[] = []
        do {
          batch = await new Promise<FileSystemEntry[]>((resolve, reject) =>
            reader.readEntries(resolve, reject)
          )
          for (const child of batch) {
            queue.push({
              entry: child,
              relParent: `${relParent}/${cur.name}`,
              depth: depth + 1,
            })
          }
        } while (batch.length > 0)
      }
    }
    return out
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
    /**
     * Current auto-reconnect attempt counter. 0 = not reconnecting;
     * 1..N = pending the Nth attempt's timer (or the Nth retry's
     * connect() call is in flight). The viewer can render
     * "Reconnecting (N/{RC_RECONNECT_LADDER_MS.length})..." while
     * `phase === 'reconnecting'`.
     */
    reconnectAttempt,
    /**
     * Whether the host has signalled (via the `rc:host_locked`
     * control-DC message) that its input desktop has transitioned
     * to `winsta0\Winlogon`. Used by the viewer to render an
     * explicit "Host locked" badge alongside the video stream's
     * padlock overlay frame.
     */
    hostLocked,
    /**
     * Name of the input desktop the agent is currently bound to,
     * as reported by the SYSTEM-context worker via the
     * `rc:desktop_changed` control-DC message. `'Default'` is the
     * normal interactive desktop; `'Winlogon'` / `'Screen-saver'`
     * are secure desktops. Older agents never emit the message and
     * the ref stays at `'Default'`, which keeps the viewer
     * rendering only the existing `hostLocked` chip.
     */
    currentDesktop,
    /**
     * rc.23 — diagnostic surface. `agentLogs` holds the last
     * `rc:logs-fetch.reply` (null until first fetch); the UI renders
     * `lines` in a scrolling pre-block. `agentLogsLoading` flips
     * true while a request is in flight (drives a spinner).
     * `fetchAgentLogs(linesCount)` triggers a new request — operator
     * calls this from a toolbar button or auto-fetches when the log
     * dialog opens.
     */
    agentLogs,
    agentLogsLoading,
    fetchAgentLogs,
    attachInput,
    /** Wire `key_text` over the input DC (used by mobile keyboard +
     *  IME composition path). See [`sendKeyText`] for the full
     *  contract. */
    sendKeyText,
    /** Wire a HID `key` event over the input DC (used by mobile
     *  keyboard's special-key toolbar). See [`sendKey`] for the
     *  bitfield encoding. */
    sendKey,
    sendClipboardToAgent,
    getAgentClipboard,
    sendCtrlAltDel,
    uploadFile,
    uploadFiles,
    walkFolderEntry,
    downloadFile,
    downloadFolder,
    cancelDownload,
    cancelUpload,
    cancelTransfer,
    listDir,
    transfers,
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
    webcodecsActive,
    webcodecsCanvasEl,
    mediaIntrinsicW,
    mediaIntrinsicH,
    videoTransport,
    setVideoTransport,
    vp9_444Supported,
    vp9_444Active,
    vp9_444FramesDecoded,
    vp9_444CanvasEl,
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
  // Punctuation row. HID usages from "Keyboard/Keypad" Page (0x07).
  // These mostly reach the agent via KeyText now (printable + no
  // chord — see onKey), but we still need HID codes for the chord
  // path: e.g. Ctrl+, in some IDEs binds to "settings", which only
  // works if we forward the keypress with the chord modifier rather
  // than typing a literal ','. Without these mappings, those chords
  // were silently dropped pre-fix.
  if (code === 'Backquote') return 0x35
  if (code === 'Minus') return 0x2d
  if (code === 'Equal') return 0x2e
  if (code === 'BracketLeft') return 0x2f
  if (code === 'BracketRight') return 0x30
  if (code === 'Backslash') return 0x31
  if (code === 'Semicolon') return 0x33
  if (code === 'Quote') return 0x34
  if (code === 'Comma') return 0x36
  if (code === 'Period') return 0x37
  if (code === 'Slash') return 0x38
  if (code === 'IntlBackslash') return 0x64
  // Lock + system keys.
  if (code === 'CapsLock') return 0x39
  if (code === 'NumLock') return 0x53
  if (code === 'ScrollLock') return 0x47
  if (code === 'PrintScreen') return 0x46
  if (code === 'Pause') return 0x48
  if (code === 'ContextMenu') return 0x65
  // Numeric keypad (HID 0x53..0x63). agent's hid_to_key currently
  // falls through to Key::Other(code) for these; works enough that
  // chords with NumLock-off arrows make it through.
  if (code === 'NumpadDivide') return 0x54
  if (code === 'NumpadMultiply') return 0x55
  if (code === 'NumpadSubtract') return 0x56
  if (code === 'NumpadAdd') return 0x57
  if (code === 'NumpadEnter') return 0x58
  if (code === 'NumpadDecimal') return 0x63
  if (code.startsWith('Numpad') && code.length === 7) {
    const d = code.charCodeAt(6) - '0'.charCodeAt(0)
    // HID Numpad 1..9 → 0x59..0x61, Numpad 0 → 0x62.
    if (d === 0) return 0x62
    if (d >= 1 && d <= 9) return 0x59 + d - 1
  }
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

/**
 * Outcome of routing a `KeyboardEvent` to either the layout-agnostic
 * `KeyText` path or the existing HID `Key` path. Pure function so the
 * decision tree is unit-testable without standing up the full
 * composable.
 *
 * `text`  — printable single character with no real-chord modifiers
 *           active. Forwarded to the agent as
 *           `InputMsg::KeyText { text }` → `enigo.text` → VK_PACKET on
 *           Windows. Layout-agnostic on the remote.
 * `key`   — chord, named key (Enter/F1/ArrowUp), Tab, or any printable
 *           release that already had its keydown emitted as `text`.
 *           Forwarded as `InputMsg::Key { code, down, mods }`.
 * `drop`  — IME composition events, printable keyup whose keydown was
 *           already a `text` (the agent press+releases atomically),
 *           and unmapped codes.
 */
export type KeyDecision =
  | { kind: 'text'; text: string }
  | { kind: 'key'; code: number; down: boolean; mods: number }
  | { kind: 'drop' }

/**
 * Decide which wire-format message (if any) to emit for a browser
 * `KeyboardEvent`. Encapsulates:
 *   - IME composition guard (drop)
 *   - AltGr-aware "real chord" classification
 *   - Printable single-char → `KeyText` (layout-agnostic)
 *   - Tab carve-out (stays HID for focus traversal)
 *   - Suppress keyup for printable+nochord (matched keydown was atomic)
 *   - Fallback to HID via `kbdCodeToHid`
 *
 * Exported for vitest. The `getModifierState` parameter is split out
 * so tests can drive `AltGraph` without needing a real DOM event
 * (KeyboardEventInit doesn't accept getModifierState in jsdom).
 */
export function decideKeyAction(
  ev: Pick<
    KeyboardEvent,
    'key' | 'code' | 'ctrlKey' | 'altKey' | 'metaKey' | 'shiftKey' | 'isComposing' | 'keyCode'
  >,
  down: boolean,
  getModifierState: (key: string) => boolean = () => false,
): KeyDecision {
  // IME composition: drop. Forwarding would double-type when the
  // matching `compositionend` text flows through.
  if (ev.isComposing || ev.keyCode === 229) return { kind: 'drop' }

  const altGr = getModifierState('AltGraph')
  const realChord =
    (ev.ctrlKey && !altGr) || (ev.altKey && !altGr) || ev.metaKey
  // Tab is excluded from the printable path on purpose: many remote
  // apps gate focus traversal on WM_KEYDOWN(VK_TAB) and wouldn't pick
  // up a U+0009 typed via VK_PACKET.
  const isPrintableSingleChar =
    !realChord && ev.key.length === 1 && ev.key !== '\t'

  if (down && isPrintableSingleChar) {
    return { kind: 'text', text: ev.key }
  }
  if (!down && isPrintableSingleChar) {
    // KeyText is press+release atomic on the agent — no release event.
    return { kind: 'drop' }
  }

  const code = kbdCodeToHid(ev.code)
  if (code === null) return { kind: 'drop' }
  const mods =
    (ev.ctrlKey ? 1 : 0) | (ev.shiftKey ? 2 : 0) | (ev.altKey ? 4 : 0) | (ev.metaKey ? 8 : 0)
  return { kind: 'key', code, down, mods }
}

export { browserButton, kbdCodeToHid }
