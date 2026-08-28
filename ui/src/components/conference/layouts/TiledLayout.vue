<template>
  <div class="tiled-layout" :style="gridStyle">
    <VideoTile
      v-for="p in visibleParticipants"
      :key="p.streamKey"
      :stream="p.stream"
      :display-name="p.displayName"
      :is-muted="p.isMuted"
      :is-local="p.isLocal"
      :is-pinned="p.isPinned"
      :is-active-speaker="p.streamKey === activeSpeakerKey"
      :object-fit="p.isLocal && selfViewMode === 'in-grid-uncropped' ? 'contain' : 'cover'"
      :show-actions="true"
      :stream-key="p.streamKey"
      @toggle-pin="$emit('toggle-pin', p.streamKey)"
      @request-pip="$emit('request-pip', p.streamKey)"
      @spotlight="$emit('spotlight', p.streamKey)"
    />

    <!-- Floating self-view -->
    <div
      v-if="selfViewFloating && selfParticipant"
      ref="floatingRef"
      class="floating-self-view"
      :style="floatingStyle"
      @mousedown.prevent="startDrag"
    >
      <VideoTile
        :stream="selfParticipant.stream"
        :display-name="selfParticipant.displayName"
        :is-muted="selfParticipant.isMuted"
        :is-local="true"
        :compact="true"
        object-fit="contain"
      />
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref } from 'vue'
import VideoTile from '@/components/conference/VideoTile.vue'
import type { LayoutParticipant, SelfViewMode } from '@/composables/useConferenceLayout'

const props = defineProps<{
  participants: LayoutParticipant[]
  selfParticipant: LayoutParticipant | null
  selfViewFloating: boolean
  selfViewMode: SelfViewMode
  activeSpeakerKey: string | null
}>()

defineEmits<{
  'toggle-pin': [streamKey: string]
  'request-pip': [streamKey: string]
  spotlight: [streamKey: string]
}>()

const visibleParticipants = computed(() => props.participants)

const columnCount = computed(() => {
  const n = visibleParticipants.value.length
  if (n <= 1) return 1
  if (n <= 4) return 2
  if (n <= 9) return 3
  if (n <= 16) return 4
  if (n <= 25) return 5
  if (n <= 36) return 6
  return 7
})

const gridStyle = computed(() => ({
  display: 'grid',
  gridTemplateColumns: `repeat(${columnCount.value}, 1fr)`,
  gap: '4px',
  padding: '4px',
  width: '100%',
  height: '100%',
  alignContent: 'center',
}))

// Floating self-view dragging
const floatingRef = ref<HTMLElement | null>(null)
const floatingPos = ref({ x: -1, y: -1 }) // -1 means use default (bottom-right)
let dragStart = { x: 0, y: 0, startX: 0, startY: 0 }
let isDragging = false

const floatingStyle = computed(() => {
  if (floatingPos.value.x < 0) {
    return { bottom: '16px', right: '16px' }
  }
  return {
    top: `${floatingPos.value.y}px`,
    left: `${floatingPos.value.x}px`,
  }
})

function startDrag(e: MouseEvent) {
  isDragging = true
  const rect = floatingRef.value?.getBoundingClientRect()
  if (!rect) return
  dragStart = {
    x: e.clientX,
    y: e.clientY,
    startX: rect.left,
    startY: rect.top,
  }
  document.addEventListener('mousemove', onDrag)
  document.addEventListener('mouseup', stopDrag)
}

function onDrag(e: MouseEvent) {
  if (!isDragging) return
  floatingPos.value = {
    x: dragStart.startX + (e.clientX - dragStart.x),
    y: dragStart.startY + (e.clientY - dragStart.y),
  }
}

function stopDrag() {
  isDragging = false
  document.removeEventListener('mousemove', onDrag)
  document.removeEventListener('mouseup', stopDrag)
}
</script>

<style scoped>
.tiled-layout {
  position: relative;
  overflow: hidden;
  background: #000;
}

.floating-self-view {
  position: absolute;
  width: 200px;
  z-index: 10;
  cursor: grab;
  border-radius: 8px;
  overflow: hidden;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.5);
}

.floating-self-view:active {
  cursor: grabbing;
}
</style>
