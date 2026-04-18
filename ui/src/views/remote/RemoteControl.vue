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
        <div v-if="!rc.hasMedia.value" class="no-media-overlay">
          <v-icon size="72" color="grey-lighten-1">mdi-video-off</v-icon>
          <p class="text-body-1 mt-3">Connected — waiting for agent to publish a video track.</p>
          <p class="text-caption text-medium-emphasis mt-1">
            The agent needs to be built with the media feature
            (<code>--features media</code>) to send video.
            Input events flow as soon as the input channel is open.
          </p>
        </div>
        <!-- Synthetic cursor with the controller's initials. Hidden native
             cursor over the surface (cursor: none) so this is the only
             pointer indicator; floats at the last pointermove position. -->
        <div
          v-if="cursorVisible && controllerInitials"
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
import { useRemoteControl } from '@/composables/useRemoteControl'

const route = useRoute()
const tenantId = computed(() => route.params.tenantId as string)
const agentId = computed(() => route.params.agentId as string)

const agentStore = useAgentStore()
const authStore = useAuthStore()
const agent = ref<Agent | null>(null)
const rc = useRemoteControl()

const videoEl = ref<HTMLVideoElement | null>(null)
const stageEl = ref<HTMLElement | null>(null)
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
watch(
  () => [rc.remoteStream.value, videoEl.value] as const,
  ([stream, el]) => {
    if (stream && el && el.srcObject !== stream) {
      el.srcObject = stream
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
</style>
