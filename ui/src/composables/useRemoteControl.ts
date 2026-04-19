import { ref, onBeforeUnmount } from 'vue'
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
  /** Controller's quality preference, persisted in localStorage. Sent to
   *  the agent over the `control` data channel whenever the user changes
   *  it *or* the channel first opens. */
  const quality = ref<RcQuality>(readStoredQuality())

  let pc: RTCPeerConnection | null = null
  /** Data channels we open proactively (per docs §5). Labels match the
   *  agent's expected routing: input/control/clipboard/files. */
  const channels: Record<string, RTCDataChannel> = {}
  const inputChannelOpen = ref(false)

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
  }

  async function connect(agentId: string, permissions = 'VIEW | INPUT | CLIPBOARD') {
    if (phase.value !== 'idle' && phase.value !== 'closed' && phase.value !== 'error') {
      return // already active
    }
    error.value = null
    sessionId.value = null
    phase.value = 'requesting'

    // Inspect what video codecs this browser can decode so the agent
    // can pick the best intersection with its own AgentCaps.codecs
    // (Phase 2 negotiation, 2B.2). Filtered to the codecs we'd ever
    // negotiate: H.264 universal, H.265 + AV1 = bandwidth wins,
    // VP9 = WebRTC-mandatory, VP8 = legacy. Browsers without
    // RTCRtpReceiver.getCapabilities (older Safari/Firefox) get an
    // empty list and the agent falls back to H.264-only.
    const browserCaps = inspectBrowserVideoCodecs()
    if (browserCaps.length > 0) {
      // Surface in the UI / logs so the operator can see what was
      // advertised — useful when debugging "why didn't H.265
      // negotiate" on a session.
      console.info('[rc] browser video codecs:', browserCaps.join(', '))
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
      // Tell the browser we care about latency, not playback smoothness.
      // Default jitterBufferTarget is adaptive (100-500 ms); for remote
      // control 50 ms is the right trade: cut perceived click-to-pixel
      // delay to the wire at the cost of occasional stutter under loss.
      // Setter is Chromium 116+ / Firefox 123+ — the try/catch is the
      // actual guard against older browsers; `in`-check was cosmetic.
      try {
        (ev.receiver as RTCRtpReceiver & { jitterBufferTarget?: number | null })
          .jitterBufferTarget = 50
      } catch {
        // Best-effort — browser will use its own adaptive default.
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
    channels.clipboard = pc.createDataChannel('clipboard', { ordered: true })
    channels.files = pc.createDataChannel('files', { ordered: true })

    installRcHandlers()

    // Flag the first open so the input pump can start queuing.
    channels.input.onopen = () => { inputChannelOpen.value = true }
    channels.input.onclose = () => { inputChannelOpen.value = false }

    // Re-send the restored quality preference as soon as the control
    // channel opens — otherwise the agent would stay at its default
    // after a page reload that had set a non-default preference.
    channels.control.onopen = () => { sendQualityPreference() }

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
      const frameRect = surface.getBoundingClientRect()
      const video = findVideo()
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

    function onKey(ev: KeyboardEvent, down: boolean) {
      const code = kbdCodeToHid(ev.code)
      if (code === null) return
      const mods =
        (ev.ctrlKey ? 1 : 0) | (ev.shiftKey ? 2 : 0) | (ev.altKey ? 4 : 0) | (ev.metaKey ? 8 : 0)
      // Whitelist-only preventDefault. The previous blanket
      // ev.preventDefault() here swallowed Cmd+Q, Ctrl+T, Ctrl+W, F5,
      // etc. — a real annoyance for controllers who want to e.g. quit
      // their own browser. We only suppress defaults for keys that
      // would otherwise navigate away from the viewer page.
      if (shouldPreventDefault(ev)) ev.preventDefault()
      sendInput({ t: 'key', code, down, mods })
    }

    function shouldPreventDefault(ev: KeyboardEvent): boolean {
      // Browser back/forward by default on Backspace (some browsers);
      // Ctrl+F triggers browser find; Tab would move focus out of the
      // viewer. These must be forwarded rather than consumed by Chrome.
      if (ev.code === 'Tab') return true
      if (ev.code === 'Backspace' && !ev.ctrlKey && !ev.altKey && !ev.metaKey) return true
      // Ctrl+F (find), Ctrl+R (reload), Ctrl+W (close tab) — swallow
      // only if the controller intends that keystroke to reach the
      // remote; for now we don't forward them at all so leave defaults
      // alone (user gets the browser behaviour).
      return false
    }
    const onKeyDown = (e: KeyboardEvent) => onKey(e, true)
    const onKeyUp = (e: KeyboardEvent) => onKey(e, false)

    // Disable the OS-native context menu so right-click forwards cleanly.
    function onContextMenu(ev: MouseEvent) { ev.preventDefault() }

    surface.addEventListener('pointermove', onPointerMove)
    surface.addEventListener('pointerdown', onPointerDown)
    surface.addEventListener('pointerup', onPointerUp)
    surface.addEventListener('wheel', onWheel, { passive: false })
    surface.addEventListener('contextmenu', onContextMenu)
    // Keys are captured on window so they fire even if the video loses focus.
    window.addEventListener('keydown', onKeyDown)
    window.addEventListener('keyup', onKeyUp)

    return () => {
      surface.removeEventListener('pointermove', onPointerMove)
      surface.removeEventListener('pointerdown', onPointerDown)
      surface.removeEventListener('pointerup', onPointerUp)
      surface.removeEventListener('wheel', onWheel)
      surface.removeEventListener('contextmenu', onContextMenu)
      window.removeEventListener('keydown', onKeyDown)
      window.removeEventListener('keyup', onKeyUp)
      if (rafHandle !== null) cancelAnimationFrame(rafHandle)
    }
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
    connect,
    disconnect,
    attachInput,
  }
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

export { browserButton, kbdCodeToHid }
