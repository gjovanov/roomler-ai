<template>
  <div v-if="recentSegments.length > 0" class="transcript-overlay">
    <TransitionGroup name="subtitle">
      <div
        v-for="seg in recentSegments"
        :key="seg.id"
        class="subtitle-line"
      >
        <span class="speaker-label">{{ seg.speaker_name }}</span>
        <span class="subtitle-text">{{ seg.text }}</span>
      </div>
    </TransitionGroup>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

export interface TranscriptSegment {
  id: string
  user_id: string
  speaker_name: string
  text: string
  language?: string
  confidence?: number
  start_time: number
  end_time: number
  inference_duration_ms?: number
  received_at: number
}

const props = defineProps<{
  segments: TranscriptSegment[]
  /** How long (ms) to show each subtitle before fading. Default 5000. */
  displayDuration?: number
}>()

const displayMs = computed(() => props.displayDuration ?? 5000)

const recentSegments = computed(() => {
  const now = Date.now()
  return props.segments.filter(
    (seg) => now - seg.received_at < displayMs.value,
  )
})
</script>

<style scoped>
.transcript-overlay {
  position: absolute;
  bottom: 80px;
  left: 50%;
  transform: translateX(-50%);
  max-width: 80%;
  z-index: 10;
  pointer-events: none;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
}

.subtitle-line {
  background: rgba(0, 0, 0, 0.75);
  color: white;
  padding: 6px 16px;
  border-radius: 8px;
  font-size: 16px;
  line-height: 1.4;
  text-align: center;
  max-width: 100%;
  word-break: break-word;
}

.speaker-label {
  font-weight: 600;
  margin-right: 8px;
  color: #80cbc4;
  font-size: 13px;
}

.subtitle-text {
  font-weight: 400;
}

/* Transition animation */
.subtitle-enter-active {
  transition: all 0.3s ease;
}
.subtitle-leave-active {
  transition: all 0.5s ease;
}
.subtitle-enter-from {
  opacity: 0;
  transform: translateY(10px);
}
.subtitle-leave-to {
  opacity: 0;
}
</style>
