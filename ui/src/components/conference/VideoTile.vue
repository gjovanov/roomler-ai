<template>
  <v-card class="video-tile" color="grey-darken-3" elevation="2">
    <div class="video-container" style="aspect-ratio: 16/9; position: relative; overflow: hidden">
      <!-- Video element — always in DOM and visible so Chrome autoplay works.
           The avatar overlay covers it when there's no video track. -->
      <video
        ref="videoRef"
        autoplay
        playsinline
        :muted="isLocal"
        class="w-100 h-100"
        style="object-fit: cover"
      />

      <!-- Avatar overlay when video is off (positioned on top of video) -->
      <div
        v-if="!hasVideoTrack"
        class="d-flex align-center justify-center w-100 h-100 bg-grey-darken-4"
        style="position: absolute; top: 0; left: 0; z-index: 1"
      >
        <v-avatar size="64" color="primary">
          <span class="text-h5">{{ initial }}</span>
        </v-avatar>
      </div>

      <!-- Muted indicator -->
      <v-icon
        v-if="isMuted"
        class="mute-indicator"
        color="error"
        size="20"
      >
        mdi-microphone-off
      </v-icon>
    </div>

    <!-- Name label -->
    <v-card-text class="pa-2 text-caption text-center text-truncate">
      {{ displayName }}{{ isLocal ? ' (You)' : '' }}
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { ref, watch, computed, nextTick, onMounted, onBeforeUnmount } from 'vue'

const props = defineProps<{
  stream: MediaStream | null
  displayName: string
  isMuted: boolean
  isLocal: boolean
}>()

const videoRef = ref<HTMLVideoElement | null>(null)
let recheckTimer: ReturnType<typeof setTimeout> | null = null

const initial = computed(() =>
  props.displayName ? props.displayName.charAt(0).toUpperCase() : '?',
)

// Use a ref instead of computed — native MediaStream/Track state changes
// (addtrack, readyState) are invisible to Vue's reactivity system.
const hasVideoTrack = ref(false)

function checkVideoTrack() {
  if (!props.stream) {
    hasVideoTrack.value = false
    return
  }
  // Primary check: track metadata
  const hasLiveTrack = props.stream
    .getVideoTracks()
    .some((t) => t.enabled && t.readyState === 'live')
  // Fallback: video element is actually rendering frames (videoWidth > 0)
  const isRendering = (videoRef.value?.videoWidth ?? 0) > 0
  hasVideoTrack.value = hasLiveTrack || isRendering
}

function setupTrackListeners(stream: MediaStream) {
  stream.getVideoTracks().forEach((track) => {
    track.onended = () => checkVideoTrack()
    track.onmute = () => checkVideoTrack()
    track.onunmute = () => checkVideoTrack()
  })
}

function cleanupTrackListeners(stream: MediaStream) {
  stream.getVideoTracks().forEach((track) => {
    track.onended = null
    track.onmute = null
    track.onunmute = null
  })
}

function attachStream() {
  if (recheckTimer) {
    clearTimeout(recheckTimer)
    recheckTimer = null
  }

  const el = videoRef.value
  if (el && props.stream) {
    // Set event listeners BEFORE srcObject/play so events aren't missed
    el.onplaying = () => checkVideoTrack()
    el.onloadedmetadata = () => checkVideoTrack()
    el.onresize = () => checkVideoTrack()
    el.srcObject = props.stream
    el.play().catch(() => {})
  }
  if (props.stream) {
    setupTrackListeners(props.stream)
  }
  checkVideoTrack()

  // Delayed fallback: re-check after 500ms in case all events fired
  // before listeners were attached (race condition)
  if (props.stream) {
    recheckTimer = setTimeout(() => checkVideoTrack(), 500)
  }
}

watch(
  () => props.stream,
  (newStream, oldStream) => {
    // Clean up listeners on old stream
    if (oldStream) {
      cleanupTrackListeners(oldStream)
      oldStream.onaddtrack = null
      oldStream.onremovetrack = null
    }
    // Attach to video element
    attachStream()
    // Listen for track changes on the new stream
    if (newStream) {
      newStream.onaddtrack = () => {
        setupTrackListeners(newStream)
        checkVideoTrack()
      }
      newStream.onremovetrack = () => checkVideoTrack()
    }
  },
)

// Re-trigger play when video becomes visible (overlay hides)
watch(hasVideoTrack, (val) => {
  if (val) {
    nextTick(() => videoRef.value?.play().catch(() => {}))
  }
})

onMounted(() => {
  attachStream()
  // Also listen for track changes if stream is already set
  if (props.stream) {
    props.stream.onaddtrack = () => {
      setupTrackListeners(props.stream!)
      checkVideoTrack()
    }
    props.stream.onremovetrack = () => checkVideoTrack()
  }
})

onBeforeUnmount(() => {
  if (recheckTimer) {
    clearTimeout(recheckTimer)
    recheckTimer = null
  }
  if (props.stream) {
    cleanupTrackListeners(props.stream)
    props.stream.onaddtrack = null
    props.stream.onremovetrack = null
  }
  if (videoRef.value) {
    videoRef.value.onplaying = null
    videoRef.value.onloadedmetadata = null
    videoRef.value.onresize = null
    videoRef.value.srcObject = null
  }
})
</script>

<style scoped>
.video-tile {
  border-radius: 8px;
  overflow: hidden;
}

.mute-indicator {
  position: absolute;
  bottom: 8px;
  right: 8px;
  background: rgba(0, 0, 0, 0.6);
  border-radius: 50%;
  padding: 4px;
}
</style>
