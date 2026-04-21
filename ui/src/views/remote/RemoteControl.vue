<template>
  <v-container fluid class="pa-0 remote-control-wrapper">
    <!-- Toolbar -->
    <v-toolbar density="compact" color="surface" class="px-2">
      <v-btn
        icon="mdi-arrow-left"
        variant="text"
        :to="{ name: 'admin', params: { tenantId } }"
        aria-label="Back to Agents"
      />
      <v-toolbar-title class="d-flex align-center">
        <v-icon :color="statusColor" size="small" class="mr-2">
          mdi-circle
        </v-icon>
        {{ agent?.name || 'Agent' }}
        <span v-if="agent" class="text-caption text-medium-emphasis ml-2">
          {{ agent.os }} · {{ agent.agent_version || '—' }}
        </span>
      </v-toolbar-title>
      <v-spacer />
      <v-chip
        v-if="rc.phase.value !== 'idle'"
        size="small"
        :color="phaseColor"
        variant="flat"
        class="mr-2"
      >
        {{ rc.phase.value }}
      </v-chip>
      <!-- Quality preference: persisted to localStorage; sent to the
           agent over the control data channel on change and on channel
           open. Low / Auto / High map to bitrate clamp and codec
           preference (agent side interprets; H.265/AV1 negotiation
           lands in Phase 2). -->
      <v-select
        v-model="quality"
        :items="qualityOptions"
        density="compact"
        hide-details
        variant="outlined"
        style="max-width: 140px;"
        class="mr-2"
        prepend-inner-icon="mdi-quality-high"
        aria-label="Quality preference"
      />
      <!-- Codec override: null = let the agent pick from the full
           browser∩agent intersection (recommended). Forcing a specific
           codec is for A/B comparison — "is H.265 really better than
           H.264 on this link?". Takes effect on the next Connect. -->
      <v-select
        v-model="codecOverride"
        :items="codecOptions"
        density="compact"
        hide-details
        variant="outlined"
        style="max-width: 140px;"
        class="mr-2"
        prepend-inner-icon="mdi-video-outline"
        aria-label="Codec override"
        :title="codecOverride == null
          ? 'Agent picks the best available codec'
          : `Forcing ${codecOverride.toUpperCase()} — takes effect on next Connect`"
      />
      <!-- Clipboard sync buttons. Visible only during a live session
           because the DC is only open then. Both sides require a user
           gesture — navigator.clipboard.{readText,writeText} throw
           otherwise, so they live on explicit button clicks rather
           than a background poller. -->
      <v-btn
        v-if="rc.phase.value === 'connected'"
        icon
        variant="text"
        size="small"
        class="mr-1"
        :loading="clipboardBusy"
        aria-label="Send my clipboard to the remote host"
        title="Send my clipboard → remote"
        @click="onSendClipboard"
      >
        <v-icon>mdi-content-paste</v-icon>
      </v-btn>
      <v-btn
        v-if="rc.phase.value === 'connected'"
        icon
        variant="text"
        size="small"
        class="mr-1"
        :loading="clipboardBusy"
        aria-label="Get the remote host's clipboard"
        title="Get remote clipboard → me"
        @click="onGetClipboard"
      >
        <v-icon>mdi-content-copy</v-icon>
      </v-btn>
      <!-- Ctrl+Alt+Del: the OS intercepts this key combo before the
           browser sees it, so expose an explicit toolbar button that
           emits the equivalent key sequence over the input DC. -->
      <v-btn
        v-if="rc.phase.value === 'connected'"
        icon
        variant="text"
        size="small"
        class="mr-1"
        aria-label="Send Ctrl+Alt+Del to remote"
        title="Send Ctrl+Alt+Del"
        @click="rc.sendCtrlAltDel()"
      >
        <v-icon>mdi-keyboard-outline</v-icon>
      </v-btn>
      <!-- File upload: open a native file picker and stream whatever
           the user selects to the controlled host's Downloads folder
           via the `files` DC. -->
      <v-btn
        v-if="rc.phase.value === 'connected'"
        icon
        variant="text"
        size="small"
        class="mr-2"
        :loading="uploadBusy"
        aria-label="Upload a file to the remote host"
        title="Upload file → remote"
        @click="fileInput?.click()"
      >
        <v-icon>mdi-upload</v-icon>
      </v-btn>
      <input
        ref="fileInput"
        type="file"
        style="display: none"
        @change="onFilePicked"
      />
      <v-btn
        v-if="rc.phase.value === 'idle' || rc.phase.value === 'closed' || rc.phase.value === 'error'"
        color="primary"
        variant="flat"
        prepend-icon="mdi-play"
        :disabled="!canConnect"
        @click="startSession"
      >
        Connect
      </v-btn>
      <v-btn
        v-else
        color="error"
        variant="flat"
        prepend-icon="mdi-stop"
        @click="rc.disconnect()"
      >
        Disconnect
      </v-btn>
    </v-toolbar>

    <!-- Viewer -->
    <div class="remote-stage">
      <v-alert
        v-if="rc.error.value"
        type="error"
        variant="tonal"
        class="ma-4"
        closable
        @click:close="rc.error.value = null"
      >
        {{ rc.error.value }}
      </v-alert>

      <div v-if="rc.phase.value === 'idle' || rc.phase.value === 'closed'" class="empty-state">
        <v-icon size="96" color="grey-lighten-1">mdi-desktop-classic</v-icon>
        <p class="text-body-1 mt-2">
          Click <strong>Connect</strong> to start a remote-control session.
        </p>
        <p v-if="agent && !agent.is_online" class="text-caption text-medium-emphasis">
          This agent is currently offline. The session will fail until the agent
          reconnects.
        </p>
      </div>

      <div
        v-else-if="['requesting', 'awaiting_consent', 'negotiating'].includes(rc.phase.value)"
        class="empty-state"
      >
        <v-progress-circular indeterminate size="64" />
        <p class="text-body-1 mt-4">{{ phaseLabel }}</p>
      </div>

      <div
        v-else-if="rc.phase.value === 'connected'"
        ref="stageEl"
        class="video-frame"
        tabindex="0"
        @pointermove="onStagePointerMove"
        @pointerleave="cursorVisible = false"
        @pointerenter="cursorVisible = true"
      >
        <video ref="videoEl" autoplay playsinline muted class="remote-video" />
        <!-- Live stats readout: codec + bitrate + fps. Populated from
             RTCPeerConnection.getStats() every 500 ms inside the
             composable. Pill format keeps it unobtrusive over the
             video content. Hidden until at least the codec is known. -->
        <div
          v-if="rc.hasMedia.value && statsCodecLabel"
          class="stats-readout"
          role="status"
          aria-live="polite"
        >
          <span class="stats-pill">{{ statsCodecLabel }}</span>
          <span class="stats-pill">{{ statsBitrateLabel }}</span>
          <span class="stats-pill">{{ statsFpsLabel }}</span>
        </div>
        <div v-if="!rc.hasMedia.value" class="no-media-overlay">
          <v-icon size="72" color="grey-lighten-1">mdi-video-off</v-icon>
          <p class="text-body-1 mt-3">Connected — waiting for agent to publish a video track.</p>
          <p class="text-caption text-medium-emphasis mt-1">
            The agent needs to be built with the media feature
            (<code>--features media</code>) to send video.
            Input events flow as soon as the input channel is open.
          </p>
        </div>
        <!-- Remote cursor overlay: canvas painted with the real OS
             cursor bitmap received over the `cursor` data channel
             (1E.3). Position is translated from agent-source pixels
             into viewer-local pixels using the same letterbox
             correction the input coords use. If no shape bitmap has
             arrived yet, fall back to the initials badge. -->
        <canvas
          v-if="remoteCursorVisible"
          ref="cursorCanvas"
          class="remote-cursor-canvas"
          :width="remoteCursorSize.w"
          :height="remoteCursorSize.h"
          :style="{ transform: `translate(${remoteCursorX}px, ${remoteCursorY}px)` }"
        />
        <!-- Synthetic cursor with the controller's initials. Hidden
             native cursor over the surface (cursor: none) so this is
             the only pointer indicator; floats at the last
             pointermove position. Shows when the remote cursor
             hasn't advertised yet or to mark additional controllers
             in multi-watcher sessions. -->
        <div
          v-if="!remoteCursorVisible && cursorVisible && controllerInitials"
          class="cursor-badge"
          :style="{ transform: `translate(${cursorX}px, ${cursorY}px)` }"
        >
          <div class="cursor-arrow" />
          <div class="cursor-chip">{{ controllerInitials }}</div>
        </div>
      </div>
    </div>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onBeforeUnmount, watch } from 'vue'
import { useRoute } from 'vue-router'
import { useAgentStore, type Agent } from '@/stores/agents'
import { useAuthStore } from '@/stores/auth'
import { useRemoteControl, type RcQuality, type RcPreferredCodec } from '@/composables/useRemoteControl'
import { useSnackbar } from '@/composables/useSnackbar'

const route = useRoute()
const tenantId = computed(() => route.params.tenantId as string)
const agentId = computed(() => route.params.agentId as string)

const agentStore = useAgentStore()
const authStore = useAuthStore()
const agent = ref<Agent | null>(null)
const rc = useRemoteControl()
const { showSuccess, showError } = useSnackbar()
const clipboardBusy = ref(false)

// Push the controller's local clipboard to the agent's OS clipboard.
// Driven by a toolbar button so the `navigator.clipboard.readText()`
// call happens in a user-gesture context (Chrome throws otherwise).
// Short-lived busy spinner during the round-trip; toast on
// success/failure. Fire-and-forget — the agent doesn't ack writes.
async function onSendClipboard() {
  if (clipboardBusy.value) return
  clipboardBusy.value = true
  try {
    const ok = await rc.sendClipboardToAgent()
    if (ok) {
      showSuccess('Clipboard sent to remote')
    } else {
      showError('Could not read your clipboard (permission denied?)')
    }
  } finally {
    clipboardBusy.value = false
  }
}

// Pull the agent's clipboard text and copy it into the controller's
// local clipboard. The round-trip goes: button click → send
// `clipboard:read` on the DC → await `clipboard:content` → paste
// into `navigator.clipboard.writeText`. 5 s timeout inside the
// composable; we render the error as a snackbar.
async function onGetClipboard() {
  if (clipboardBusy.value) return
  clipboardBusy.value = true
  try {
    const text = await rc.getAgentClipboard()
    try {
      await globalThis.navigator.clipboard.writeText(text)
      showSuccess(`Copied remote clipboard (${text.length} chars)`)
    } catch (e) {
      showError(`Could not write to your clipboard: ${(e as Error).message}`)
    }
  } catch (e) {
    showError(`Remote clipboard read failed: ${(e as Error).message}`)
  } finally {
    clipboardBusy.value = false
  }
}

// Template refs. Declared before the computeds / watches below that
// reference them — Vue 3 <script setup> executes top-to-bottom, and
// `watch` evaluates its source getter eagerly during setup to wire
// reactivity. Reading `cursorCanvas.value` in a watch source while
// `cursorCanvas` is still in the temporal dead zone manifests as the
// minified TDZ crash "Cannot access 'Z' before initialization" at
// setup time, which kills the whole RemoteControl page before it
// can paint. Keep template refs at the top of setup to avoid this.
const videoEl = ref<HTMLVideoElement | null>(null)
const stageEl = ref<HTMLElement | null>(null)
const cursorCanvas = ref<HTMLCanvasElement | null>(null)
const fileInput = ref<HTMLInputElement | null>(null)
const uploadBusy = ref(false)

// Stream the user-picked file to the remote's Downloads folder via
// the `files` DC. 64 KiB chunks with backpressure on
// `RTCDataChannel.bufferedAmount` so large files don't OOM the tab.
async function onFilePicked(ev: Event) {
  const input = ev.target as HTMLInputElement | null
  const f = input?.files?.[0]
  if (!f) return
  uploadBusy.value = true
  try {
    const res = await rc.uploadFile(f)
    showSuccess(`Uploaded ${f.name} → ${res.path}`)
  } catch (e) {
    showError(`Upload failed: ${(e as Error).message}`)
  } finally {
    uploadBusy.value = false
    if (input) input.value = '' // allow re-selecting the same file
  }
}

// Quality preference: v-select emits immediately on change. We proxy
// through a computed so the composable stays the source of truth
// (persists + pushes to agent). The v-select's inner value is
// whatever the composable already holds, so reloads show the
// restored preference without an extra effect.
const qualityOptions = [
  { title: 'Auto', value: 'auto' },
  { title: 'Low', value: 'low' },
  { title: 'High', value: 'high' },
] as const
const quality = computed<RcQuality>({
  get: () => rc.quality.value,
  set: (v: RcQuality) => rc.setQuality(v),
})

// Codec override: null = let the agent pick the best available. The
// value is persisted via the composable (localStorage) so it survives
// a page reload; it only takes effect on the next connect — live
// sessions keep whatever SDP they negotiated at start.
const codecOptions = [
  { title: 'Codec: Auto', value: null },
  { title: 'H.264', value: 'h264' },
  { title: 'H.265 / HEVC', value: 'h265' },
  { title: 'AV1', value: 'av1' },
  { title: 'VP9', value: 'vp9' },
] as const
const codecOverride = computed<RcPreferredCodec | null>({
  get: () => rc.preferredCodec.value,
  set: (v: RcPreferredCodec | null) => rc.setPreferredCodec(v),
})

// Stats readout formatters. Pure computeds — the composable already
// polls getStats() every 500 ms and updates rc.stats.value.
//
// The codec label is enriched with HW/SW based on the agent's
// advertised AgentCaps.hw_encoders (2A.2 wired). This makes the
// pill informative ("H.265 HW") rather than ambiguous ("H265").
const statsCodecLabel = computed(() => {
  const raw = rc.stats.value.codec
  if (!raw) return ''
  const lower = raw.toLowerCase()
  // Prettify well-known names. H264 → H.264, H265 → H.265; others
  // pass through uppercased.
  const display = lower
    .replace(/^h(\d{3})$/, (_m, n) => `H.${n}`)
    .toUpperCase()
  // Guess HW/SW from the agent's caps if available; default to SW
  // (the safe assumption — reporting HW when uncertain would
  // mislead the operator about latency expectations).
  const enc = agent.value?.capabilities?.hw_encoders ?? []
  const hasHw = enc.some(
    (e) => e.toLowerCase().includes(lower) && e.toLowerCase().includes('-hw'),
  )
  return `${display} ${hasHw ? 'HW' : 'SW'}`
})
const statsBitrateLabel = computed(() => {
  const bps = rc.stats.value.bitrate_bps
  if (bps <= 0) return '— bps'
  if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(1)} Mbps`
  return `${Math.round(bps / 1_000)} kbps`
})
const statsFpsLabel = computed(() => {
  const fps = rc.stats.value.fps
  if (fps <= 0) return '— fps'
  return `${Math.round(fps)} fps`
})

// Remote cursor overlay (1E.3). Requires both a position and a
// matching shape bitmap; hides during paint if either is missing.
const remoteCursorVisible = computed(() => {
  const pos = rc.cursor.value.pos
  if (!pos) return false
  return rc.cursor.value.shapes.has(pos.id)
})

const remoteCursorSize = computed(() => {
  const pos = rc.cursor.value.pos
  if (!pos) return { w: 0, h: 0 }
  const shape = rc.cursor.value.shapes.get(pos.id)
  if (!shape) return { w: 0, h: 0 }
  return { w: shape.bitmap.width, h: shape.bitmap.height }
})

// Translate agent-source pixels → viewer-local pixels using the
// same letterbox correction the pointer input uses, so the cursor
// lands at the exact spot on the video. `agent.value` comes from
// load step below and carries the agent's native resolution via
// its capability payload (the displays list); we fall back to the
// video element's intrinsic size otherwise.
const remoteCursorX = computed(() => {
  const pos = rc.cursor.value.pos
  if (!pos) return 0
  const shape = rc.cursor.value.shapes.get(pos.id)
  if (!shape) return 0
  const scale = letterboxScale()
  return scale.offsetX + pos.x * scale.sx - shape.hotspotX
})
const remoteCursorY = computed(() => {
  const pos = rc.cursor.value.pos
  if (!pos) return 0
  const shape = rc.cursor.value.shapes.get(pos.id)
  if (!shape) return 0
  const scale = letterboxScale()
  return scale.offsetY + pos.y * scale.sy - shape.hotspotY
})

function letterboxScale(): { sx: number; sy: number; offsetX: number; offsetY: number } {
  const stage = stageEl.value
  const video = videoEl.value
  if (!stage || !video || !video.videoWidth || !video.videoHeight) {
    return { sx: 1, sy: 1, offsetX: 0, offsetY: 0 }
  }
  const fw = stage.clientWidth
  const fh = stage.clientHeight
  const vAR = video.videoWidth / video.videoHeight
  const fAR = fw / fh
  let visibleW: number
  let visibleH: number
  let offsetX: number
  let offsetY: number
  if (vAR > fAR) {
    visibleW = fw
    visibleH = fw / vAR
    offsetX = 0
    offsetY = (fh - visibleH) / 2
  } else {
    visibleW = fh * vAR
    visibleH = fh
    offsetX = (fw - visibleW) / 2
    offsetY = 0
  }
  return {
    sx: visibleW / video.videoWidth,
    sy: visibleH / video.videoHeight,
    offsetX,
    offsetY,
  }
}

// Paint the current cursor shape onto the canvas every time the
// shape or pos changes. drawImage is cheap (O(cursor pixels), ≤32×32
// for classic cursors) so we don't need an explicit RAF loop.
watch(
  () => {
    const p = rc.cursor.value.pos
    return [p?.id ?? null, cursorCanvas.value] as const
  },
  ([id, canvas]) => {
    if (!canvas || id == null) return
    const shape = rc.cursor.value.shapes.get(id)
    if (!shape) return
    const ctx = canvas.getContext('2d')
    if (!ctx) return
    ctx.clearRect(0, 0, canvas.width, canvas.height)
    ctx.drawImage(shape.bitmap, 0, 0)
  },
  { immediate: false },
)

let detachInput: (() => void) | null = null

// Synthetic cursor overlay. The native pointer is hidden over the video
// (cursor: none in CSS below) so this badge is the only pointer indicator.
// Initials come from the logged-in controller so it stays meaningful if
// multi-watcher sessions land later (today it's 1:1, but the label is
// already user-scoped).
const cursorX = ref(0)
const cursorY = ref(0)
const cursorVisible = ref(false)
const controllerInitials = computed(() => {
  const u = authStore.user
  const src = u?.display_name || u?.username || ''
  const parts = src.trim().split(/\s+/).filter(Boolean)
  if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase()
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase()
  return ''
})
function onStagePointerMove(ev: PointerEvent) {
  const host = stageEl.value
  if (!host) return
  const rect = host.getBoundingClientRect()
  cursorX.value = ev.clientX - rect.left
  cursorY.value = ev.clientY - rect.top
}

const canConnect = computed(() => !!agent.value)
const statusColor = computed(() => (agent.value?.is_online ? 'success' : 'grey'))
const phaseColor = computed(() => {
  switch (rc.phase.value) {
    case 'connected': return 'success'
    case 'error': return 'error'
    case 'closed': return 'grey'
    default: return 'info'
  }
})
const phaseLabel = computed(() => {
  switch (rc.phase.value) {
    case 'requesting': return 'Requesting session…'
    case 'awaiting_consent': return 'Waiting for the agent to allow the connection…'
    case 'negotiating': return 'Negotiating the peer connection…'
    default: return ''
  }
})

async function loadAgent() {
  if (!agentStore.agents.length) {
    await agentStore.fetchAgents(tenantId.value)
  }
  agent.value = agentStore.agents.find((a) => a.id === agentId.value) || null
}

function startSession() {
  if (!agent.value) return
  rc.connect(agent.value.id)
}

// When the remote stream becomes available, attach it to the video element.
// Race to watch out for: ontrack can fire during `phase === 'negotiating'`
// (before the <video> element is even mounted, since it lives inside a
// v-else-if="phase === 'connected'"). A single watcher on the stream would
// see videoEl.value = null at that moment and silently skip the assignment;
// when the element mounts later no watcher re-fires. Watch both refs and
// attach whenever both are present.
let rvfcHandle: number | null = null
watch(
  () => [rc.remoteStream.value, videoEl.value] as const,
  ([stream, el]) => {
    if (stream && el && el.srcObject !== stream) {
      el.srcObject = stream
      // requestVideoFrameCallback keeps the tab "hot" against
      // Chrome's background throttling AND gives us a cheap hook to
      // recover from the video element's paused-for-optimization
      // state that sometimes triggers on identical-frame runs (e.g.
      // long idle screens).
      const elWithRvfc = el as HTMLVideoElement & {
        requestVideoFrameCallback?: (cb: (now: number, metadata: unknown) => void) => number
      }
      const rvfc = elWithRvfc.requestVideoFrameCallback
      if (typeof rvfc === 'function') {
        const tick = () => {
          if (!videoEl.value) {
            rvfcHandle = null
            return
          }
          if (videoEl.value.paused) {
            videoEl.value.play().catch(() => { /* autoplay gating — ignore */ })
          }
          rvfcHandle = (videoEl.value as typeof elWithRvfc)
            .requestVideoFrameCallback!(tick)
        }
        rvfcHandle = rvfc.call(el, tick)
      }
    }
  },
  { immediate: true },
)

// Once the connected stage mounts, wire input listeners to it. Detach
// when we leave the "connected" phase so keystrokes don't escape after
// a disconnect.
watch(
  () => [rc.phase.value, stageEl.value] as const,
  ([phase, el]) => {
    if (phase === 'connected' && el && !detachInput) {
      detachInput = rc.attachInput(el as HTMLElement)
      ;(el as HTMLElement).focus()
    } else if (phase !== 'connected' && detachInput) {
      detachInput()
      detachInput = null
    }
  },
)

onMounted(loadAgent)
onBeforeUnmount(() => {
  if (detachInput) detachInput()
  rc.disconnect()
})
</script>

<style scoped>
.remote-control-wrapper {
  height: 100%;
  display: flex;
  flex-direction: column;
}
.remote-stage {
  flex: 1;
  display: flex;
  align-items: stretch;
  justify-content: stretch;
  background: #0b0b0b;
  position: relative;
}
.empty-state {
  margin: auto;
  text-align: center;
  padding: 32px;
  color: rgba(255, 255, 255, 0.7);
}
.video-frame {
  position: relative;
  width: 100%;
  height: 100%;
  /* Hide the native OS pointer so the synthetic cursor below is the only
     thing the controller sees — matches collaborative-tool semantics. */
  cursor: none;
}
.remote-video {
  width: 100%;
  height: 100%;
  object-fit: contain;
  background: #000;
}
.remote-cursor-canvas {
  position: absolute;
  top: 0;
  left: 0;
  pointer-events: none;
  z-index: 2;
  image-rendering: pixelated;
  will-change: transform;
}
.cursor-badge {
  position: absolute;
  top: 0;
  left: 0;
  pointer-events: none;
  z-index: 2;
  /* translate is applied inline from (cursorX, cursorY). Offset the
     arrow tip to the exact pointer hotspot (top-left of the arrow). */
  will-change: transform;
}
.cursor-arrow {
  width: 0;
  height: 0;
  border-left: 14px solid #4fc3f7;
  border-top: 14px solid transparent;
  border-bottom: 14px solid transparent;
  filter: drop-shadow(0 1px 2px rgba(0, 0, 0, 0.45));
  transform: rotate(-20deg);
  transform-origin: 0 0;
}
.cursor-chip {
  position: absolute;
  top: 14px;
  left: 10px;
  background: #4fc3f7;
  color: #0b2530;
  font: 600 11px/1 system-ui, sans-serif;
  padding: 2px 6px;
  border-radius: 8px 8px 8px 2px;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.4);
  letter-spacing: 0.5px;
  white-space: nowrap;
}
.no-media-overlay {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.6);
  color: #fff;
  text-align: center;
  padding: 24px;
}
.stats-readout {
  position: absolute;
  top: 8px;
  right: 8px;
  display: flex;
  gap: 6px;
  pointer-events: none;
  z-index: 3;
}
.stats-pill {
  background: rgba(0, 0, 0, 0.55);
  color: rgba(255, 255, 255, 0.9);
  font: 500 11px/1 ui-monospace, "SF Mono", Menlo, monospace;
  padding: 4px 8px;
  border-radius: 999px;
  letter-spacing: 0.3px;
  backdrop-filter: blur(4px);
}
</style>
