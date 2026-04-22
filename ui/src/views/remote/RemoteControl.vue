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
      <!-- Scale mode: how the remote video is rendered in the local
           viewer. "Adaptive" is the prior default (object-fit: contain);
           "Original" is 1:1 intrinsic pixels with scroll when larger
           than the viewport; "Custom" lets the user dial 5-1000%. -->
      <v-select
        v-model="scaleMode"
        :items="scaleOptions"
        density="compact"
        hide-details
        variant="outlined"
        style="max-width: 160px;"
        class="mr-2"
        prepend-inner-icon="mdi-image-size-select-actual"
        aria-label="View scale"
      />
      <!-- Only visible in Custom mode — dials the CSS zoom level. -->
      <v-text-field
        v-if="scaleMode === 'custom'"
        v-model.number="scalePercent"
        type="number"
        min="5"
        max="1000"
        step="5"
        density="compact"
        hide-details
        variant="outlined"
        style="max-width: 110px;"
        class="mr-2"
        suffix="%"
        aria-label="Custom scale percent"
      />
      <!-- Remote resolution selector: what the AGENT captures + encodes.
           `original` = native monitor; `fit` = match our viewport CSS
           px × dpr (auto-updates on stage resize); `custom…` opens a
           dialog. Changing this sends an rc:resolution control-DC
           message; agent rebuilds its encoder on the next frame. -->
      <v-select
        v-model="resolutionPresetValue"
        :items="resolutionOptions"
        density="compact"
        hide-details
        variant="outlined"
        style="max-width: 190px;"
        class="mr-2"
        prepend-inner-icon="mdi-monitor-screenshot"
        aria-label="Remote capture resolution"
        :title="resolutionButtonTitle"
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
      <!-- Fullscreen toggle: requests fullscreen on the stage element.
           ESC exits natively — no custom keybinding needed. Icon flips
           via the document.fullscreenchange listener. -->
      <v-btn
        v-if="rc.phase.value === 'connected'"
        icon
        variant="text"
        size="small"
        class="mr-2"
        :aria-label="isFullscreen ? 'Exit fullscreen' : 'Enter fullscreen'"
        :title="isFullscreen ? 'Exit fullscreen (Esc)' : 'Fullscreen'"
        @click="toggleFullscreen"
      >
        <v-icon>{{ isFullscreen ? 'mdi-fullscreen-exit' : 'mdi-fullscreen' }}</v-icon>
      </v-btn>
      <!-- Low-latency (WebCodecs) toggle. When supported + enabled, the
           viewer uses a Web Worker + VideoDecoder + canvas render path
           that bypasses Chrome's built-in jitter buffer (~80 ms soft
           floor on <video>). Takes effect on the next Connect. Disabled
           and off when the browser lacks RTCRtpScriptTransform /
           VideoDecoder. -->
      <v-btn
        icon
        variant="text"
        size="small"
        class="mr-2"
        :color="webcodecsOn ? 'primary' : undefined"
        :disabled="!rc.webcodecsSupported.value"
        :aria-label="webcodecsOn ? 'Disable low-latency path' : 'Enable low-latency (WebCodecs) path'"
        :title="webcodecsTooltip"
        @click="toggleWebCodecs"
      >
        <v-icon>{{ webcodecsOn ? 'mdi-flash' : 'mdi-flash-outline' }}</v-icon>
      </v-btn>
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
        :class="`scale-${rc.scaleMode.value}`"
        tabindex="0"
        @pointermove="onStagePointerMove"
        @pointerleave="cursorVisible = false"
        @pointerenter="cursorVisible = true"
      >
        <!-- Classic render path: <video> bound to the remote MediaStream.
             Used unless the viewer opted into the WebCodecs path AND
             the browser supports it. We still render the <video>
             element in WebCodecs mode but hide it — input + cursor
             math hang off `rc.mediaIntrinsicW/H` which the composable
             keeps in sync either way. -->
        <video
          v-show="!isWebCodecsRender"
          ref="videoEl"
          autoplay
          playsinline
          muted
          class="remote-video"
          :class="`scale-${rc.scaleMode.value}`"
          :style="videoScaleStyle"
        />
        <!-- Low-latency render path: canvas fed by the Worker-driven
             VideoDecoder. transferControlToOffscreen() happens once
             per session in the composable; this element is the main-
             thread handle we bind the canvas ref on. Same scale
             classes + style as the video so the existing layout +
             cursor overlays keep working. -->
        <canvas
          v-if="isWebCodecsRender"
          :ref="bindWebcodecsCanvas"
          class="remote-video webcodecs-canvas"
          :class="`scale-${rc.scaleMode.value}`"
          :style="videoScaleStyle"
        />
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
    <!-- Custom-resolution dialog. Opened when the operator picks the
         "Custom…" option in the Resolution dropdown; submits an
         rc:resolution {mode:'custom'} message on confirm. -->
    <v-dialog v-model="customResolutionDialog" max-width="480">
      <v-card>
        <v-card-title>Custom remote resolution</v-card-title>
        <v-card-text>
          <div class="d-flex align-center mb-3">
            <v-text-field
              v-model.number="customResolutionW"
              type="number"
              min="160"
              max="7680"
              step="10"
              density="compact"
              hide-details
              variant="outlined"
              label="Width"
              class="mr-2"
            />
            <span class="text-medium-emphasis mr-2">×</span>
            <v-text-field
              v-model.number="customResolutionH"
              type="number"
              min="120"
              max="4320"
              step="10"
              density="compact"
              hide-details
              variant="outlined"
              label="Height"
            />
          </div>
          <v-chip-group column>
            <v-chip
              v-for="p in customResolutionPresets"
              :key="`${p.w}x${p.h}`"
              size="small"
              variant="outlined"
              @click="pickCustomResolutionPreset(p.w, p.h)"
            >
              {{ p.w }} × {{ p.h }}{{ p.note ? ` — ${p.note}` : '' }}
            </v-chip>
          </v-chip-group>
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="customResolutionDialog = false">Cancel</v-btn>
          <v-btn
            color="primary"
            variant="flat"
            :disabled="!customResolutionValid"
            @click="confirmCustomResolution"
          >
            Apply
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onBeforeUnmount, watch } from 'vue'
import { useRoute } from 'vue-router'
import { useAgentStore, type Agent } from '@/stores/agents'
import { useAuthStore } from '@/stores/auth'
import {
  useRemoteControl,
  type RcQuality,
  type RcPreferredCodec,
  type RcScaleMode,
  type RcResolutionSetting,
  type RcRenderPath,
} from '@/composables/useRemoteControl'
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

// Scale mode + custom percent. Proxy through a computed so the
// composable stays the source of truth (persists across reloads).
const scaleOptions = [
  { title: 'Adaptive', value: 'adaptive' },
  { title: 'Original', value: 'original' },
  { title: 'Custom…', value: 'custom' },
] as const
const scaleMode = computed<RcScaleMode>({
  get: () => rc.scaleMode.value,
  set: (v: RcScaleMode) => rc.setScaleMode(v),
})
const scalePercent = computed<number>({
  get: () => rc.scaleCustomPercent.value,
  set: (v: number) => rc.setScaleCustomPercent(v),
})

// Intrinsic remote-frame dimensions. The composable is the source of
// truth — it's fed by `<video>.onresize` in classic mode and by the
// WebCodecs worker's `first-frame` message in the low-latency path.
// That way any consumer downstream (scale style, coord math, cursor
// overlay) reads one set of refs regardless of render path.
const videoIntrinsicW = rc.mediaIntrinsicW
const videoIntrinsicH = rc.mediaIntrinsicH

// Render-path toggle. The composable persists the preference across
// reloads; `webcodecsSupported` is true only when the browser exposes
// both RTCRtpScriptTransform + VideoDecoder (Chrome 94+). The
// `isWebCodecsRender` computed drives template rendering — it's only
// true when the session is actively using the WebCodecs path (the
// user opted in AND the browser supports it). We read `rc.renderPath`
// directly so the UI state matches what the next Connect would do.
const webcodecsOn = computed<boolean>(() => rc.renderPath.value === 'webcodecs')
const isWebCodecsRender = computed<boolean>(
  () => webcodecsOn.value && rc.webcodecsSupported.value,
)
const webcodecsTooltip = computed<string>(() => {
  if (!rc.webcodecsSupported.value) {
    return 'Low-latency (WebCodecs) path requires Chrome 94+ — not supported in this browser'
  }
  return webcodecsOn.value
    ? 'Low-latency (WebCodecs) ON — bypasses <video> jitter buffer. Takes effect on next Connect.'
    : 'Low-latency (WebCodecs) OFF — using <video> render path'
})
function toggleWebCodecs() {
  const next: RcRenderPath = webcodecsOn.value ? 'video' : 'webcodecs'
  rc.setRenderPath(next)
}
/** Bind callback for the webcodecs canvas ref. Vue calls this with
 *  the element (or null on unmount) — we forward to the composable's
 *  writable canvas ref so `pc.ontrack` can see it. */
function bindWebcodecsCanvas(el: Element | unknown) {
  rc.webcodecsCanvasEl.value = (el as HTMLCanvasElement | null) ?? null
}

// Fullscreen toggle. Drives the stage element into/out of the browser's
// Fullscreen API. `isFullscreen` tracks the real DOM state via the
// fullscreenchange event so ESC (which the browser handles natively)
// updates the icon without us polling.
const isFullscreen = ref(false)
function toggleFullscreen() {
  const el = stageEl.value
  if (!el) return
  if (document.fullscreenElement) {
    void document.exitFullscreen().catch(() => { /* user cancelled; ignore */ })
  } else {
    void el.requestFullscreen().catch(() => { /* user gesture / API missing; ignore */ })
  }
}
function onFullscreenChange() {
  isFullscreen.value = document.fullscreenElement !== null
}

// Inline style for the <video> element. In `original` and `custom`
// modes we set explicit pixel dims so the outer `.video-frame` can
// detect overflow and show scrollbars; the `<video>` element's
// `width: auto` default is unreliable inside a flex container. In
// `adaptive` mode the CSS class handles sizing (100%/100% +
// object-fit: contain).
const videoScaleStyle = computed<Record<string, string> | undefined>(() => {
  const w = videoIntrinsicW.value
  const h = videoIntrinsicH.value
  if (!Number.isFinite(w) || !Number.isFinite(h) || w <= 0 || h <= 0) return undefined
  if (rc.scaleMode.value === 'custom') {
    const pct = rc.scaleCustomPercent.value / 100
    return { width: `${w * pct}px`, height: `${h * pct}px` }
  }
  if (rc.scaleMode.value === 'original') {
    return { width: `${w}px`, height: `${h}px` }
  }
  return undefined
})

// -----------------------------------------------------------------
// Remote resolution (Phase 2 of the viewer-controls sprint)
// -----------------------------------------------------------------

// The v-select value is a discriminator string — not the full
// RcResolutionSetting — because v-select items need primitive values
// for equality. `original` / `fit` map directly; `custom` maps to
// `custom:<w>x<h>` for display and opens a dialog when picked so the
// operator can edit dims.
const resolutionOptions = computed(() => {
  const opts: { title: string; value: string }[] = [
    { title: 'Original resolution', value: 'original' },
    { title: 'Fit to local viewport', value: 'fit' },
  ]
  if (rc.resolution.value.mode === 'custom') {
    const w = rc.resolution.value.width ?? 0
    const h = rc.resolution.value.height ?? 0
    opts.push({ title: `Custom: ${w} × ${h}`, value: 'custom-current' })
  }
  opts.push({ title: 'Custom…', value: 'custom-edit' })
  return opts
})

const resolutionPresetValue = computed<string>({
  get: () => {
    if (rc.resolution.value.mode === 'original') return 'original'
    if (rc.resolution.value.mode === 'fit') return 'fit'
    return 'custom-current'
  },
  set: (v) => {
    if (v === 'original') {
      rc.setResolution({ mode: 'original' })
    } else if (v === 'fit') {
      applyFitResolution()
    } else if (v === 'custom-edit') {
      // Seed the dialog from the current values (or the stage's
      // dimensions if we have none yet) so the user isn't starting
      // from a blank field.
      const cur = rc.resolution.value
      customResolutionW.value = cur.width ?? 1920
      customResolutionH.value = cur.height ?? 1080
      customResolutionDialog.value = true
    }
    // 'custom-current' is a noop — it's only used as the v-select's
    // "display the existing custom dims" slot.
  },
})

const resolutionButtonTitle = computed(() => {
  const s = rc.resolution.value
  if (s.mode === 'original') return 'Agent streams at native monitor resolution'
  if (s.mode === 'fit') {
    return `Agent downscales to fit local viewport (currently ${s.width ?? '?'} × ${s.height ?? '?'})`
  }
  return `Custom: ${s.width ?? '?'} × ${s.height ?? '?'}`
})

const customResolutionDialog = ref(false)
const customResolutionW = ref(1920)
const customResolutionH = ref(1080)
const customResolutionPresets: Array<{ w: number; h: number; note?: string }> = [
  { w: 1280, h: 720, note: '720p' },
  { w: 1920, h: 1080, note: '1080p' },
  { w: 1920, h: 1200, note: 'WUXGA' },
  { w: 2560, h: 1440, note: '1440p' },
  { w: 2560, h: 1600, note: 'WQXGA' },
  { w: 3840, h: 2160, note: '4K UHD' },
]
const customResolutionValid = computed(() => {
  const w = customResolutionW.value
  const h = customResolutionH.value
  return (
    Number.isFinite(w) && Number.isFinite(h) &&
    w >= 160 && w <= 7680 &&
    h >= 120 && h <= 4320
  )
})
function pickCustomResolutionPreset(w: number, h: number) {
  customResolutionW.value = w
  customResolutionH.value = h
}
function confirmCustomResolution() {
  if (!customResolutionValid.value) return
  const setting: RcResolutionSetting = {
    mode: 'custom',
    width: Math.round(customResolutionW.value),
    height: Math.round(customResolutionH.value),
  }
  rc.setResolution(setting)
  customResolutionDialog.value = false
}

/** Apply Fit mode using the current stage dimensions × devicePixelRatio
 *  — captures "what fits in my browser right now at its native pixel
 *  density". Also re-emitted on stage resize via `ResizeObserver`
 *  below, debounced 250 ms so drag-resize doesn't churn the encoder. */
function applyFitResolution() {
  const el = stageEl.value
  if (!el) return
  const rect = el.getBoundingClientRect()
  const dpr = window.devicePixelRatio || 1
  const w = Math.max(1, Math.round(rect.width * dpr))
  const h = Math.max(1, Math.round(rect.height * dpr))
  rc.setResolution({ mode: 'fit', width: w, height: h })
}

// ResizeObserver on the stage so Fit mode tracks viewport changes.
// Debounced — drag-resize fires dozens of events per second and each
// rc:resolution change rebuilds the encoder on the agent side.
let fitResizeTimer: ReturnType<typeof setTimeout> | null = null
let fitResizeObserver: ResizeObserver | null = null
function startFitResizeObserver() {
  if (fitResizeObserver || !stageEl.value || !('ResizeObserver' in window)) return
  fitResizeObserver = new ResizeObserver(() => {
    if (rc.resolution.value.mode !== 'fit') return
    if (fitResizeTimer) clearTimeout(fitResizeTimer)
    fitResizeTimer = setTimeout(() => {
      applyFitResolution()
    }, 250)
  })
  fitResizeObserver.observe(stageEl.value)
}
function stopFitResizeObserver() {
  if (fitResizeTimer) {
    clearTimeout(fitResizeTimer)
    fitResizeTimer = null
  }
  if (fitResizeObserver) {
    fitResizeObserver.disconnect()
    fitResizeObserver = null
  }
}

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
  const scale = cursorMapping()
  return scale.offsetX + pos.x * scale.sx - shape.hotspotX
})
const remoteCursorY = computed(() => {
  const pos = rc.cursor.value.pos
  if (!pos) return 0
  const shape = rc.cursor.value.shapes.get(pos.id)
  if (!shape) return 0
  const scale = cursorMapping()
  return scale.offsetY + pos.y * scale.sy - shape.hotspotY
})

/** Map an agent-source pixel coordinate to the logical coordinate
 *  space of `.video-frame` (which the cursor canvas + synthetic
 *  badge are positioned inside). Scale-mode aware:
 *
 *  - `adaptive`: `<video>` fills the frame with `object-fit: contain`.
 *    Use the original letterbox math to find the scale factor + any
 *    letterbox padding.
 *  - `original`: video at intrinsic 1:1, anchored to the frame's
 *    top-left (flex: flex-start). Scale = 1, no offsets. If the frame
 *    is scrolled the transform stays in logical space → visually
 *    tracks the content.
 *  - `custom`: scale = `scalePercent / 100`, no offsets (same flex
 *    anchor). */
function cursorMapping(): { sx: number; sy: number; offsetX: number; offsetY: number } {
  const stage = stageEl.value
  const video = videoEl.value
  if (!stage || !video || !video.videoWidth || !video.videoHeight) {
    return { sx: 1, sy: 1, offsetX: 0, offsetY: 0 }
  }
  if (rc.scaleMode.value === 'original') {
    return { sx: 1, sy: 1, offsetX: 0, offsetY: 0 }
  }
  if (rc.scaleMode.value === 'custom') {
    const pct = rc.scaleCustomPercent.value / 100
    return { sx: pct, sy: pct, offsetX: 0, offsetY: 0 }
  }
  // Adaptive: the video fills the frame with object-fit: contain,
  // producing letterbox bars on the axis where aspect ratios disagree.
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
  // `transform: translate()` on the badge/canvas is in the logical
  // coordinate space of `.video-frame` — the space that includes
  // scroll offset. `ev.clientX - rect.left` gives viewport-relative
  // offset from the frame's *visible* left edge; add scrollLeft to
  // reach logical space so the overlay tracks the pointer after
  // scrolling in Original / Custom modes.
  cursorX.value = ev.clientX - rect.left + host.scrollLeft
  cursorY.value = ev.clientY - rect.top + host.scrollTop
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
// Keep our intrinsic-dimension refs in sync with the actual video
// element. `resize` fires on every resolution change from the agent
// (docking, DPI flip, rc:resolution control message in Phase 2);
// `loadedmetadata` covers the first-frame bootstrap. In WebCodecs
// mode the <video> element never receives decoded frames (the
// receiver transform swallows them), so `videoWidth` stays 0 — we
// skip writing zeros here to avoid clobbering the worker's first-
// frame dims. The worker's `first-frame` message is the authoritative
// source in that mode.
function refreshVideoDims(el: HTMLVideoElement) {
  if (isWebCodecsRender.value) return
  videoIntrinsicW.value = el.videoWidth || 0
  videoIntrinsicH.value = el.videoHeight || 0
}
watch(
  () => [rc.remoteStream.value, videoEl.value] as const,
  ([stream, el]) => {
    if (stream && el && el.srcObject !== stream) {
      el.srcObject = stream
      // Track intrinsic video size so `custom` scale mode can compute
      // pixel dimensions + the coordinate mapper can fall back cleanly.
      el.addEventListener('loadedmetadata', () => refreshVideoDims(el))
      el.addEventListener('resize', () => refreshVideoDims(el))
      refreshVideoDims(el)
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
      // Start watching the stage for size changes so Fit mode
      // auto-updates the agent's target resolution.
      startFitResizeObserver()
      // If the restored preference was Fit (from localStorage) the
      // stored width/height are from the previous session — re-emit
      // with the current window size so the agent uses today's dims.
      if (rc.resolution.value.mode === 'fit') applyFitResolution()
    } else if (phase !== 'connected' && detachInput) {
      detachInput()
      detachInput = null
      stopFitResizeObserver()
    }
  },
)

onMounted(() => {
  void loadAgent()
  document.addEventListener('fullscreenchange', onFullscreenChange)
})
onBeforeUnmount(() => {
  if (detachInput) detachInput()
  stopFitResizeObserver()
  document.removeEventListener('fullscreenchange', onFullscreenChange)
  // Exit fullscreen on unmount so navigating away doesn't leave the
  // browser in a weird fullscreen state.
  if (document.fullscreenElement) void document.exitFullscreen().catch(() => {})
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
  /* `min-height: 0` is required for the flex parent (`.remote-stage`
     with `align-items: stretch`) to allow this child to actually
     shrink to the available space. Without it, a large intrinsic
     child (e.g. a 4K <video> in Original mode) could balloon the
     frame past the viewport — showing a cropped view instead of
     scrollbars. `min-width: 0` is the horizontal counterpart. */
  min-width: 0;
  min-height: 0;
  /* Hide the native OS pointer so the synthetic cursor below is the only
     thing the controller sees — matches collaborative-tool semantics. */
  cursor: none;
}
/* Scrollable frame for the scale modes where the video can overflow
   the viewport (original = 1:1 always if source > viewer, custom ≥
   100%). `block` display is intentional — a flex container would
   impose item-sizing rules and some browsers shrink the flex item
   even with `flex: none` when the container's overflow engages.
   Block + explicit child pixel dims (via :style) is the simplest
   path that reliably shows scrollbars. */
.video-frame.scale-original,
.video-frame.scale-custom {
  overflow: auto;
  display: block;
}
.remote-video {
  background: #000;
  display: block;
}
.remote-video.scale-adaptive {
  width: 100%;
  height: 100%;
  object-fit: contain;
}
.remote-video.scale-original,
.remote-video.scale-custom {
  /* Explicit pixel dims come from the :style binding. object-fit:
     fill stretches to exactly the CSS-declared dimensions with no
     letterbox, which is what we want for 1:1 Original and the
     user-driven Custom scale. */
  object-fit: fill;
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
