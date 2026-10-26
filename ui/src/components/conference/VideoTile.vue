<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-card
    ref="tileRef"
    class="video-tile"
    :class="{
      'active-speaker-border': isActiveSpeaker,
      'compact-tile': compact,
    }"
    color="grey-darken-3"
    elevation="2"
    @dblclick="$emit('spotlight', streamKey)"
  >
    <div
      class="video-container"
      style="aspect-ratio: 16/9; position: relative; overflow: hidden"
      @click="onTileTap"
    >
      <!-- Video element — always in DOM and visible so Chrome autoplay works.
           The avatar overlay covers it when there's no video track.
           FR-124 — `soundBlocked`: the browser refused to start it with sound,
           so it plays muted until the next gesture. A binding, not just an
           assignment, so a re-render can't unmute it behind the browser's back. -->
      <video
        ref="videoRef"
        autoplay
        playsinline
        :muted="isLocal || soundBlocked"
        class="w-100 h-100"
        :style="{ objectFit: objectFit }"
        :data-stream-key="streamKey"
      />

      <!-- Avatar overlay when video is off (positioned on top of video).
           FR-30 P3 — `videoPaused` is the PEER's own word for it. Without it a
           camera-off participant showed their last frame, frozen, forever:
           the track keeps reading live+unmuted on this side, so `hasVideoTrack`
           cannot tell. A frozen face is worse than an avatar — it looks live. -->
      <div
        v-if="!hasVideoTrack || videoPaused"
        class="d-flex align-center justify-center w-100 h-100 bg-grey-darken-4"
        style="position: absolute; top: 0; left: 0; z-index: 1"
      >
        <v-avatar :size="compact ? 40 : 64" color="primary">
          <span :class="compact ? 'text-body-2' : 'text-h5'">{{ initial }}</span>
        </v-avatar>
      </div>

      <!-- FR-124 — a picture that plays with nothing in it (a covered lens, a
           closed privacy shutter) says so, in text. A black tile and a tile
           that never started looked the same, and a covered webcam got blamed
           on the other device of the same account. -->
      <div
        v-if="showBlackNotice"
        class="black-picture-notice d-flex flex-column align-center justify-center text-center"
        data-testid="black-picture-notice"
      >
        <v-icon :size="compact ? 18 : 28" color="warning">mdi-camera-off-outline</v-icon>
        <div class="text-caption font-weight-medium">
          {{ compact ? 'Black picture' : isLocal ? 'Your camera is sending a black picture' : 'Black picture from this camera' }}
        </div>
        <div v-if="!compact" class="text-caption text-medium-emphasis">
          {{ isLocal ? 'Is the lens covered?' : 'Its lens may be covered' }}
        </div>
      </div>

      <!-- FR-124 — the browser refused to start this tile. Its picture plays
           muted meanwhile; the next gesture anywhere, or this button, brings
           the sound back. -->
      <v-btn
        v-if="playBlocked"
        class="play-blocked-btn"
        size="small"
        variant="flat"
        color="rgba(0,0,0,0.7)"
        prepend-icon="mdi-play"
        data-testid="tap-to-play"
        @click.stop="resumePlayback"
      >
        Tap to play
      </v-btn>
      <v-btn
        v-else-if="soundBlocked"
        class="sound-blocked-btn"
        size="x-small"
        variant="flat"
        color="rgba(0,0,0,0.7)"
        prepend-icon="mdi-volume-off"
        data-testid="tap-for-sound"
        @click.stop="resumePlayback"
      >
        Tap for sound
      </v-btn>

      <!-- Pin badge -->
      <v-icon
        v-if="isPinned"
        class="pin-badge"
        color="primary"
        size="18"
      >
        mdi-pin
      </v-icon>

      <!-- Camera-off indicator (FR-30 P3) -->
      <v-icon
        v-if="videoPaused"
        class="camera-indicator"
        color="error"
        size="20"
      >
        mdi-video-off
      </v-icon>

      <!-- Muted indicator -->
      <v-icon
        v-if="isMuted"
        class="mute-indicator"
        color="error"
        size="20"
      >
        mdi-microphone-off
      </v-icon>

      <!-- Action overlay: shown on hover with a mouse, by a tap on a touch
           screen (which has no hover). Inert while hidden — it used to sit
           over the whole tile at opacity 0, so a tap in the middle hit an
           invisible picture-in-picture button. -->
      <div
        v-if="showActions"
        class="action-overlay d-flex align-center justify-center ga-1"
        :class="{ 'action-overlay--open': touchActionsOpen }"
      >
        <v-btn
          icon
          size="small"
          variant="flat"
          color="rgba(0,0,0,0.6)"
          @click.stop="$emit('toggle-pin', streamKey)"
        >
          <v-icon size="18">{{ isPinned ? 'mdi-pin-off' : 'mdi-pin' }}</v-icon>
        </v-btn>
        <v-btn
          v-if="pipSupported"
          icon
          size="small"
          variant="flat"
          color="rgba(0,0,0,0.6)"
          :title="isPiP ? 'Leave picture-in-picture' : 'Picture-in-picture'"
          @click.stop="$emit('request-pip', streamKey)"
        >
          <v-icon size="18">mdi-picture-in-picture-bottom-right</v-icon>
        </v-btn>
        <!-- FR-25 — per-tile fullscreen. Hidden where the browser has no
             element fullscreen (iOS Safari) rather than shipping a dead
             control that silently does nothing. -->
        <v-btn
          v-if="fullscreenSupported"
          icon
          size="small"
          variant="flat"
          color="rgba(0,0,0,0.6)"
          :title="isFullscreen ? 'Exit fullscreen' : 'Fullscreen'"
          @click.stop="toggleFullscreen"
        >
          <v-icon size="18">{{ isFullscreen ? 'mdi-fullscreen-exit' : 'mdi-fullscreen' }}</v-icon>
        </v-btn>
      </div>
    </div>

    <!-- Name label -->
    <v-card-text
      class="pa-2 text-caption text-center text-truncate"
      :class="{ 'py-1': compact }"
    >
      {{ displayName }}{{ isLocal ? ' (You)' : '' }}
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { ref, watch, computed, nextTick, onMounted, onBeforeUnmount } from 'vue'
import { watchBlackPicture } from '@/utils/blackPicture'

const props = withDefaults(defineProps<{
  stream: MediaStream | null
  displayName: string
  isMuted: boolean
  isLocal: boolean
  isPinned?: boolean
  isActiveSpeaker?: boolean
  objectFit?: 'cover' | 'contain'
  compact?: boolean
  showActions?: boolean
  streamKey?: string
  /** FR-30 P3 — the peer said their camera is off. Not derivable here: their
      track reads live+unmuted on this side either way. */
  videoPaused?: boolean
  /** FR-124 — a shared screen. A black slide is not a covered camera, so its
      picture is never judged. */
  isScreenShare?: boolean
}>(), {
  isPinned: false,
  isActiveSpeaker: false,
  objectFit: 'cover',
  compact: false,
  showActions: false,
  streamKey: '',
  videoPaused: false,
  isScreenShare: false,
})

const emit = defineEmits<{
  'toggle-pin': [streamKey: string]
  'request-pip': [streamKey: string]
  /** FR-25 — double-click: make this tile the spotlight (toggles). */
  spotlight: [streamKey: string]
  /** FR-124 — this tile's picture turned black, or stopped being black. Sent
      once per change, and `false` on unmount if it was black. */
  'picture-black': [black: boolean]
}>()

const videoRef = ref<HTMLVideoElement | null>(null)

// FR-25 — per-tile fullscreen. `$el` because v-card is a component; the
// document-level listener keeps the icon honest when the user leaves
// fullscreen with Esc rather than the button.
const tileRef = ref<{ $el?: HTMLElement } | null>(null)
const fullscreenSupported = typeof document !== 'undefined' && !!document.fullscreenEnabled
// Like the call toolbar's own PiP button (`usePictureInPicture().isSupported`):
// only where the browser has it. Elsewhere it was a dead control that answered
// with "Picture-in-picture is disabled in this browser".
const pipSupported = typeof document !== 'undefined' && !!document.pictureInPictureEnabled

// FR-116 — a touch screen has no hover: a tap on the tile shows its actions,
// another tap (or one on the dimmed background) hides them. The buttons stop
// their own clicks, so pressing one never toggles the overlay.
const hoverNone = typeof window !== 'undefined' && !!window.matchMedia?.('(hover: none)').matches
const touchActionsOpen = ref(false)
function onTileTap() {
  if (hoverNone && props.showActions) touchActionsOpen.value = !touchActionsOpen.value
}
const isFullscreen = ref(false)
const isPiP = computed(() =>
  typeof document !== 'undefined' && document.pictureInPictureElement === videoRef.value,
)

function onFullscreenChange() {
  const el = tileRef.value?.$el
  isFullscreen.value = !!el && document.fullscreenElement === el
}

async function toggleFullscreen() {
  const el = tileRef.value?.$el
  if (!el) return
  try {
    if (document.fullscreenElement === el) {
      await document.exitFullscreen()
    } else {
      await el.requestFullscreen()
    }
  } catch {
    // Denied (permissions policy / not user-activated) — the icon state
    // is driven by fullscreenchange, so nothing to unwind here.
  }
}
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

// FR-124 — the browser refused to START this tile (its autoplay policy:
// audible playback before the page has had a gesture). Muted playback is
// always allowed, so the picture comes back muted at once and the sound waits
// for the next gesture. Both refusals used to be swallowed by
// `.catch(() => {})`, which left the tile black until its stream changed.
const soundBlocked = ref(false)
const playBlocked = ref(false)
let gestureArmed = false

async function startPlayback() {
  const el = videoRef.value
  if (!el || !props.stream) return
  try {
    // Called first and synchronously, so a call from inside a gesture plays
    // inside that gesture.
    await el.play()
    playBlocked.value = false
  } catch (err) {
    const name = (err as { name?: string } | null)?.name
    // A newer srcObject or play() took over; that one reports for itself.
    if (name === 'AbortError') return
    if (name !== 'NotAllowedError') {
      console.warn('[VideoTile] play() failed:', err)
      return
    }
    if (!el.muted) {
      soundBlocked.value = true
      el.muted = true // now, not at the next render: the retry below needs it
      try {
        await el.play()
        playBlocked.value = false
      } catch {
        playBlocked.value = true
      }
    } else {
      playBlocked.value = true
    }
    armGestureRetry()
  }
}

/** From inside a gesture: give the tile its sound back and start it. */
function resumePlayback() {
  const el = videoRef.value
  if (!el) return
  if (soundBlocked.value) {
    soundBlocked.value = false
    el.muted = props.isLocal
  }
  void startPlayback()
}

function onGesture() {
  disarmGestureRetry()
  resumePlayback()
}

// `click`, not `pointerdown`: on a touch screen the activation comes with the
// pointer's release, so a play() at pointerdown would be refused again.
// Capture phase, so a click a control stops on its way still counts.
function armGestureRetry() {
  if (gestureArmed || typeof document === 'undefined') return
  gestureArmed = true
  document.addEventListener('click', onGesture, true)
  document.addEventListener('keydown', onGesture, true)
}

function disarmGestureRetry() {
  if (!gestureArmed) return
  gestureArmed = false
  document.removeEventListener('click', onGesture, true)
  document.removeEventListener('keydown', onGesture, true)
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
    void startPlayback()
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
    nextTick(() => void startPlayback())
  }
})

// FR-124 — is this tile's picture black? Judged only while it can be a
// camera's fault: a playing camera track that is enabled (your own camera
// turned off renders black on purpose), live, and receiving (`muted` = no
// media arriving), from a peer who hasn't said their camera is off (FR-30),
// and never a shared screen.
const pictureBlack = ref(false)
let stopBlackWatch: (() => void) | null = null

function cameraTrackLive(): boolean {
  return !!props.stream?.getVideoTracks().some((t) => t.enabled && t.readyState === 'live' && !t.muted)
}

const showBlackNotice = computed(
  () => pictureBlack.value && hasVideoTrack.value && !props.videoPaused && !props.isScreenShare,
)

onMounted(() => {
  attachStream()
  if (fullscreenSupported) document.addEventListener('fullscreenchange', onFullscreenChange)
  // Also listen for track changes if stream is already set
  if (props.stream) {
    props.stream.onaddtrack = () => {
      setupTrackListeners(props.stream!)
      checkVideoTrack()
    }
    props.stream.onremovetrack = () => checkVideoTrack()
  }
  if (videoRef.value) {
    stopBlackWatch = watchBlackPicture(videoRef.value, {
      isActive: () =>
        !props.isScreenShare && !props.videoPaused && hasVideoTrack.value && cameraTrackLive(),
      onChange: (black) => {
        pictureBlack.value = black
        emit('picture-black', black)
      },
    })
  }
})

onBeforeUnmount(() => {
  // Before anything else, while the parent still listens: a black tile
  // that goes away must say so, or the call page keeps its warning up.
  stopBlackWatch?.()
  stopBlackWatch = null
  disarmGestureRetry()
  if (fullscreenSupported) document.removeEventListener('fullscreenchange', onFullscreenChange)
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

// Expose video ref for PiP
defineExpose({ videoRef })
</script>

<style scoped>
.video-tile {
  border-radius: 8px;
  overflow: hidden;
  transition: border-color 0.3s ease;
  border: 2px solid transparent;
}

.active-speaker-border {
  border-color: rgb(var(--v-theme-primary));
}

.compact-tile .video-container {
  aspect-ratio: 16/9;
}

.pin-badge {
  position: absolute;
  top: 6px;
  left: 6px;
  background: rgba(0, 0, 0, 0.6);
  border-radius: 50%;
  padding: 2px;
  z-index: 2;
}

.camera-indicator {
  position: absolute;
  bottom: 8px;
  /* Left of the mic badge: camera-off and mic-off happen together often
     enough that they must not sit on top of each other. */
  right: 40px;
  background: rgba(0, 0, 0, 0.6);
  border-radius: 50%;
  padding: 4px;
}

.mute-indicator {
  position: absolute;
  bottom: 8px;
  right: 8px;
  background: rgba(0, 0, 0, 0.6);
  border-radius: 50%;
  padding: 4px;
  z-index: 2;
}

.action-overlay {
  position: absolute;
  top: 0;
  left: 0;
  width: 100%;
  height: 100%;
  background: rgba(0, 0, 0, 0.4);
  opacity: 0;
  transition: opacity 0.2s ease;
  z-index: 3;
}

.action-overlay {
  pointer-events: none;
}
.video-container:hover .action-overlay,
.action-overlay--open {
  opacity: 1;
  pointer-events: auto;
}
/* A touch screen's sticky hover must not reveal (or arm) it: the tap does. */
@media (hover: none) {
  .video-container:hover .action-overlay:not(.action-overlay--open) {
    opacity: 0;
    pointer-events: none;
  }
}

/* FR-124 — over the black picture, under the hover actions: it explains, it
   takes no clicks. */
.black-picture-notice {
  position: absolute;
  inset: 0;
  gap: 2px;
  padding: 8px;
  color: white;
  pointer-events: none;
  z-index: 2;
}

/* Above the hover actions (z 3): the one control that brings the tile back
   has to stay reachable while a mouse is over the tile. */
.play-blocked-btn {
  position: absolute;
  top: 50%;
  left: 50%;
  transform: translate(-50%, -50%);
  z-index: 4;
}

.sound-blocked-btn {
  position: absolute;
  bottom: 8px;
  left: 8px;
  z-index: 4;
}
</style>
