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

interface IceServer {
  urls: string[]
  username?: string
  credential?: string
}

interface TurnCredsResponse {
  ice_servers: IceServer[]
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

  let pc: RTCPeerConnection | null = null
  /** Data channels we open proactively (per docs §5). Labels match the
   *  agent's expected routing: input/control/clipboard/files. */
  const channels: Record<string, RTCDataChannel> = {}
  const inputChannelOpen = ref(false)

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
      if (!remoteStream.value) remoteStream.value = new MediaStream()
      remoteStream.value.addTrack(ev.track)
      hasMedia.value = true
      // Tell the browser we care about latency, not playback smoothness.
      // Default jitterBufferTarget is adaptive (100-500 ms); for remote
      // control 50 ms is the right trade: cut perceived click-to-pixel
      // delay to the wire at the cost of occasional stutter under loss.
      // NB: setter is relatively new (Chromium 116+, Firefox 123+), so
      // feature-detect rather than crash on older browsers.
      try {
        const r = ev.receiver as RTCRtpReceiver & { jitterBufferTarget?: number | null }
        if ('jitterBufferTarget' in r) r.jitterBufferTarget = 50
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
      else if (state === 'closed' && phase.value !== 'error') {
        phase.value = 'closed'
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

    // Kick off the rc:* handshake.
    ws.sendRaw({
      t: 'rc:session.request',
      agent_id: agentId,
      permissions,
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
      const vw = video?.videoWidth ?? 0
      const vh = video?.videoHeight ?? 0

      // Before the first decoded frame we can't know the aspect ratio —
      // fall back to frame-relative coordinates so tests still function.
      if (!vw || !vh) {
        const x = (ev.clientX - frameRect.left) / Math.max(frameRect.width, 1)
        const y = (ev.clientY - frameRect.top) / Math.max(frameRect.height, 1)
        return { x: clamp01(x), y: clamp01(y), insideVideo: true }
      }

      // Compute the letterboxed region `object-fit: contain` produces.
      const videoAR = vw / vh
      const frameAR = frameRect.width / Math.max(frameRect.height, 1)
      let visibleW: number, visibleH: number, offsetX: number, offsetY: number
      if (videoAR > frameAR) {
        // Wider than the frame — black bars top/bottom.
        visibleW = frameRect.width
        visibleH = frameRect.width / videoAR
        offsetX = 0
        offsetY = (frameRect.height - visibleH) / 2
      } else {
        // Taller than the frame — black bars left/right.
        visibleW = frameRect.height * videoAR
        visibleH = frameRect.height
        offsetX = (frameRect.width - visibleW) / 2
        offsetY = 0
      }

      const localX = ev.clientX - frameRect.left - offsetX
      const localY = ev.clientY - frameRect.top - offsetY
      const insideVideo =
        localX >= 0 && localX <= visibleW && localY >= 0 && localY <= visibleH
      return {
        x: clamp01(localX / Math.max(visibleW, 1)),
        y: clamp01(localY / Math.max(visibleH, 1)),
        insideVideo,
      }
    }

    function onPointerMove(ev: PointerEvent) {
      const { x, y, insideVideo } = normalisedXY(ev)
      if (!insideVideo) return
      pendingMove = { x, y, mon: 0 }
      if (rafHandle === null) rafHandle = requestAnimationFrame(flushPendingMove)
    }

    function onPointerDown(ev: PointerEvent) {
      ev.preventDefault()
      const { x, y, insideVideo } = normalisedXY(ev)
      if (!insideVideo) return
      surface.setPointerCapture(ev.pointerId)
      sendInput({ t: 'mouse_button', btn: browserButton(ev.button), down: true, x, y, mon: 0 })
    }

    function onPointerUp(ev: PointerEvent) {
      try { surface.releasePointerCapture(ev.pointerId) } catch { /* noop */ }
      const { x, y, insideVideo } = normalisedXY(ev)
      if (!insideVideo) return
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
      // Trap reserved combos so the browser doesn't navigate away.
      ev.preventDefault()
      const code = kbdCodeToHid(ev.code)
      if (code === null) return
      const mods =
        (ev.ctrlKey ? 1 : 0) | (ev.shiftKey ? 2 : 0) | (ev.altKey ? 4 : 0) | (ev.metaKey ? 8 : 0)
      sendInput({ t: 'key', code, down, mods })
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
    connect,
    disconnect,
    attachInput,
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

export { browserButton, kbdCodeToHid }
