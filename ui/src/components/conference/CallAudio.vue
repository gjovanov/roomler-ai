<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- #3563 — the call's sound. One hidden <audio> per remote stream key that
       carries audio, for as long as the call lasts, whatever the layout
       renders: a participant past the tile cap, one hidden by "Hide
       participants without video", one beyond the mini view's three while the
       call page is away — each is heard. The tiles show pictures only. -->
  <div class="call-audio">
    <!-- `muted` is a binding, not only an assignment, so a re-render cannot
         unmute a sink behind the browser's back while its sound waits for a
         gesture. -->
    <audio
      v-for="sink in sinks"
      :key="sink.key"
      :ref="(el) => bindSink(sink.key, el as HTMLAudioElement | null)"
      autoplay
      :muted="blocked.has(sink.key)"
      :data-stream-key="sink.key"
      :data-user-id="sink.userId"
    />

    <!-- FR-124 — the browser refused to start the call's sound (its autoplay
         policy: audible playback before a gesture). Said once, for the whole
         call; the next click or key anywhere, or this button, brings every
         participant back. -->
    <v-btn
      v-if="soundBlocked"
      class="call-sound-blocked-btn"
      size="small"
      variant="flat"
      color="rgba(0,0,0,0.7)"
      prepend-icon="mdi-volume-off"
      data-testid="tap-for-sound"
      @click="restoreSound"
    >
      Tap for sound
    </v-btn>
  </div>
</template>

<script setup lang="ts">
import { computed, onBeforeUnmount, reactive, shallowRef, watch } from 'vue'
import { useConferenceStore } from '@/stores/conference'

const conferenceStore = useConferenceStore()

interface Sink {
  key: string
  userId: string
}

/** What is rendered: one entry per remote stream key that carries audio. */
const sinks = shallowRef<Sink[]>([])

/** Each sink's own audio-only stream, and the ids of the tracks in it. The
 *  store REPLACES a key's MediaStream every time a consumer joins it (the
 *  camera after the microphone, or the reverse), so a sink bound to the
 *  store's stream would restart its sound for a video track. This one is
 *  rebuilt only when the key's audio tracks change. */
const sinkStreams = new Map<string, { stream: MediaStream; trackIds: string }>()
const elements = new Map<string, HTMLAudioElement>()

/** The sinks whose audible start the browser refused. They play muted until
 *  the next gesture, which keeps the pipeline (and the speaker detection that
 *  reads it) running. One flag for the call, not one per participant. */
const blocked = reactive(new Set<string>())
const soundBlocked = computed(() => blocked.size > 0)
let gestureArmed = false

function liveAudioTracks(stream: MediaStream): MediaStreamTrack[] {
  // A closed consumer's track ends but can stay in the key's stream (only a
  // shared screen's key is deleted when its producer closes).
  return stream.getAudioTracks().filter((t) => t.readyState !== 'ended')
}

function syncSinks() {
  const next: Sink[] = []
  for (const [key, remote] of conferenceStore.remoteStreams) {
    const tracks = liveAudioTracks(remote.stream)
    if (tracks.length === 0) continue
    const trackIds = tracks
      .map((t) => t.id)
      .sort()
      .join(',')
    const current = sinkStreams.get(key)
    if (!current) {
      // A participant who arrives while the call's sound waits for a gesture
      // starts muted and is brought back by that same gesture: their element
      // is new, and nothing tells us the browser would let it play aloud.
      if (soundBlocked.value) blocked.add(key)
      sinkStreams.set(key, { stream: new MediaStream(tracks), trackIds })
    } else if (current.trackIds !== trackIds) {
      sinkStreams.set(key, { stream: new MediaStream(tracks), trackIds })
      const el = elements.get(key)
      if (el) attach(key, el)
    }
    next.push({ key, userId: remote.userId })
  }
  const live = new Set(next.map((s) => s.key))
  for (const key of [...sinkStreams.keys()]) {
    if (!live.has(key)) sinkStreams.delete(key)
  }
  for (const key of [...blocked]) {
    if (!live.has(key)) blocked.delete(key)
  }
  if (blocked.size === 0) disarmGestureRetry()
  sinks.value = next
}

/** Called by Vue with the element on every render of its sink, and with
 *  `null` when the sink goes (a participant left, the call ended). */
function bindSink(key: string, el: HTMLAudioElement | null) {
  if (el) {
    elements.set(key, el)
    attach(key, el)
    return
  }
  const gone = elements.get(key)
  elements.delete(key)
  // Dropping the source stops the element and frees its player. The tracks
  // are the store's: they are never stopped here, or the tiles and the
  // speaker detection that read them would go silent too.
  if (gone) gone.srcObject = null
}

function attach(key: string, el: HTMLAudioElement) {
  const entry = sinkStreams.get(key)
  if (!entry || el.srcObject === entry.stream) return
  el.srcObject = entry.stream
  void startSink(key, el)
}

async function startSink(key: string, el: HTMLAudioElement) {
  try {
    // Called first and synchronously, so a call from inside a gesture plays
    // inside that gesture.
    await el.play()
  } catch (err) {
    const name = (err as { name?: string } | null)?.name
    // A newer srcObject or play() took over; that one reports for itself.
    if (name === 'AbortError') return
    if (name !== 'NotAllowedError') {
      console.warn('[CallAudio] play() failed:', err)
      return
    }
    if (elements.get(key) !== el) return
    blocked.add(key)
    el.muted = true // now, not at the next render: the retry below needs it
    armGestureRetry()
    try {
      await el.play()
    } catch (retryErr) {
      // Refused even muted (a browser set to block all media): it stays in
      // `blocked`, and the gesture starts it.
      const retryName = (retryErr as { name?: string } | null)?.name
      if (retryName !== 'NotAllowedError' && retryName !== 'AbortError') {
        console.warn('[CallAudio] play() failed:', retryErr)
      }
    }
  }
}

/** From inside a gesture: give every refused sink its sound back. */
function restoreSound() {
  disarmGestureRetry()
  const keys = [...blocked]
  blocked.clear()
  for (const key of keys) {
    const el = elements.get(key)
    if (!el) continue
    el.muted = false
    void startSink(key, el)
  }
}

// `click`, not `pointerdown`: on a touch screen the activation comes with the
// pointer's release, so a play() at pointerdown would be refused again.
// Capture phase, so a click a control stops on its way still counts.
function armGestureRetry() {
  if (gestureArmed || typeof document === 'undefined') return
  gestureArmed = true
  document.addEventListener('click', restoreSound, true)
  document.addEventListener('keydown', restoreSound, true)
}

function disarmGestureRetry() {
  if (!gestureArmed) return
  gestureArmed = false
  document.removeEventListener('click', restoreSound, true)
  document.removeEventListener('keydown', restoreSound, true)
}

// Every change of the store's map: a key added or deleted, or its entry
// replaced by `consumeProducer` with a stream that holds one more track.
watch(
  () => [...conferenceStore.remoteStreams.values()].map((r) => r.stream),
  syncSinks,
  { immediate: true },
)

onBeforeUnmount(() => {
  disarmGestureRetry()
  for (const el of elements.values()) el.srcObject = null
  elements.clear()
  sinkStreams.clear()
  blocked.clear()
})
</script>

<style scoped>
/* Fixed, so it shows over the call page and over any other page while the
   mini view plays the call. Below the app bar, above the page and the nav
   drawer, under dialogs. */
.call-sound-blocked-btn {
  position: fixed;
  top: 72px;
  left: 50%;
  transform: translateX(-50%);
  z-index: 1010;
}
</style>
