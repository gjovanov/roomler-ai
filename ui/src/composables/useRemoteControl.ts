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
    sendInput({ t: 'MouseMove', ...pendingMove })
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
    function normalisedXY(ev: PointerEvent | MouseEvent | WheelEvent): { x: number; y: number } {
      const rect = surface.getBoundingClientRect()
      const x = (ev.clientX - rect.left) / Math.max(rect.width, 1)
      const y = (ev.clientY - rect.top) / Math.max(rect.height, 1)
      return { x: Math.min(Math.max(x, 0), 1), y: Math.min(Math.max(y, 0), 1) }
    }

    function onPointerMove(ev: PointerEvent) {
      const { x, y } = normalisedXY(ev)
      pendingMove = { x, y, mon: 0 }
      if (rafHandle === null) rafHandle = requestAnimationFrame(flushPendingMove)
    }

    function onPointerDown(ev: PointerEvent) {
      ev.preventDefault()
      surface.setPointerCapture(ev.pointerId)
      const { x, y } = normalisedXY(ev)
      sendInput({ t: 'MouseButton', btn: browserButton(ev.button), down: true, x, y, mon: 0 })
    }

    function onPointerUp(ev: PointerEvent) {
      try { surface.releasePointerCapture(ev.pointerId) } catch { /* noop */ }
      const { x, y } = normalisedXY(ev)
      sendInput({ t: 'MouseButton', btn: browserButton(ev.button), down: false, x, y, mon: 0 })
    }

    function onWheel(ev: WheelEvent) {
      ev.preventDefault()
      // Browser uses positive Y for down; agent does the same.
      sendInput({
        t: 'MouseWheel',
        dx: ev.deltaX,
        dy: ev.deltaY,
        mode: ev.deltaMode === 0 ? 'Pixel' : ev.deltaMode === 1 ? 'Line' : 'Page',
      })
    }

    function onKey(ev: KeyboardEvent, down: boolean) {
      // Trap reserved combos so the browser doesn't navigate away.
      ev.preventDefault()
      const code = kbdCodeToHid(ev.code)
      if (code === null) return
      const mods =
        (ev.ctrlKey ? 1 : 0) | (ev.shiftKey ? 2 : 0) | (ev.altKey ? 4 : 0) | (ev.metaKey ? 8 : 0)
      sendInput({ t: 'Key', code, down, mods })
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
function browserButton(n: number): string {
  switch (n) {
    case 0: return 'Left'
    case 1: return 'Middle'
    case 2: return 'Right'
    case 3: return 'Back'
    case 4: return 'Forward'
    default: return 'Left'
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
