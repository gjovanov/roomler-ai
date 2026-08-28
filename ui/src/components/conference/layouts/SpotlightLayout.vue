<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <div class="spotlight-layout">
    <!-- Primary area -->
    <div class="spotlight-primary" :style="primaryGridStyle">
      <VideoTile
        v-for="p in primary"
        :key="p.streamKey"
        :stream="p.stream"
        :display-name="p.displayName"
        :is-muted="p.isMuted"
        :is-local="p.isLocal"
        :is-pinned="p.isPinned"
        :is-active-speaker="p.streamKey === activeSpeakerKey"
        object-fit="contain"
        :show-actions="true"
        @toggle-pin="$emit('toggle-pin', p.streamKey)"
        @request-pip="$emit('request-pip', p.streamKey)"
      />
    </div>

    <!-- Filmstrip -->
    <div v-if="secondary.length > 0" class="spotlight-filmstrip">
      <VideoTile
        v-for="p in secondary"
        :key="p.streamKey"
        :stream="p.stream"
        :display-name="p.displayName"
        :is-muted="p.isMuted"
        :is-local="p.isLocal"
        :is-pinned="p.isPinned"
        :is-active-speaker="p.streamKey === activeSpeakerKey"
        :compact="true"
        :show-actions="true"
        @toggle-pin="$emit('toggle-pin', p.streamKey)"
        @request-pip="$emit('request-pip', p.streamKey)"
      />
    </div>

    <!-- Floating self-view -->
    <div
      v-if="selfViewFloating && selfParticipant"
      class="floating-self-view"
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
import { computed } from 'vue'
import VideoTile from '@/components/conference/VideoTile.vue'
import type { LayoutParticipant } from '@/composables/useConferenceLayout'

const props = defineProps<{
  primary: LayoutParticipant[]
  secondary: LayoutParticipant[]
  selfParticipant: LayoutParticipant | null
  selfViewFloating: boolean
  activeSpeakerKey: string | null
}>()

defineEmits<{
  'toggle-pin': [streamKey: string]
  'request-pip': [streamKey: string]
}>()

const primaryGridStyle = computed(() => {
  const n = props.primary.length
  if (n <= 1) return {}
  const cols = n <= 2 ? 2 : 3
  return {
    display: 'grid',
    gridTemplateColumns: `repeat(${cols}, 1fr)`,
    gap: '4px',
  }
})
</script>

<style scoped>
.spotlight-layout {
  display: flex;
  flex-direction: column;
  width: 100%;
  height: 100%;
  background: #000;
  position: relative;
  overflow: hidden;
}

.spotlight-primary {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 4px;
  min-height: 0;
}

.spotlight-filmstrip {
  height: 120px;
  flex-shrink: 0;
  display: flex;
  gap: 4px;
  padding: 4px;
  overflow-x: auto;
  overflow-y: hidden;
  background: rgba(0, 0, 0, 0.3);
}

.spotlight-filmstrip > * {
  flex: 0 0 180px;
  height: 100%;
}

.floating-self-view {
  position: absolute;
  bottom: 136px;
  right: 16px;
  width: 200px;
  z-index: 10;
  border-radius: 8px;
  overflow: hidden;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.5);
}
</style>
