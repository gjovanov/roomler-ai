<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <div class="sidebar-layout">
    <!-- Primary area -->
    <div class="sidebar-primary">
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

    <!-- Sidebar strip -->
    <div v-if="secondary.length > 0" class="sidebar-strip">
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
import VideoTile from '@/components/conference/VideoTile.vue'
import type { LayoutParticipant } from '@/composables/useConferenceLayout'

defineProps<{
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
</script>

<style scoped>
.sidebar-layout {
  display: grid;
  grid-template-columns: 1fr 280px;
  width: 100%;
  height: 100%;
  background: #000;
  position: relative;
  overflow: hidden;
}

.sidebar-primary {
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 4px;
  min-width: 0;
}

.sidebar-strip {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 4px;
  overflow-y: auto;
  overflow-x: hidden;
  background: rgba(0, 0, 0, 0.3);
}

.sidebar-strip > * {
  flex-shrink: 0;
}

.floating-self-view {
  position: absolute;
  bottom: 16px;
  left: 16px;
  width: 200px;
  z-index: 10;
  border-radius: 8px;
  overflow: hidden;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.5);
}
</style>
