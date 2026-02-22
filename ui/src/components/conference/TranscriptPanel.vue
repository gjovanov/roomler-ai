<template>
  <div class="transcript-panel d-flex flex-column">
    <v-toolbar density="compact" flat>
      <v-toolbar-title class="text-body-1">
        {{ $t('call.transcript') }}
      </v-toolbar-title>
      <v-spacer />
      <v-menu>
        <template #activator="{ props: menuProps }">
          <v-btn icon="mdi-download" variant="text" size="small" v-bind="menuProps" />
        </template>
        <v-list density="compact">
          <v-list-item @click="downloadVtt">
            <v-list-item-title>{{ $t('call.downloadVtt') }}</v-list-item-title>
          </v-list-item>
          <v-list-item @click="downloadSrt">
            <v-list-item-title>{{ $t('call.downloadSrt') }}</v-list-item-title>
          </v-list-item>
        </v-list>
      </v-menu>
    </v-toolbar>
    <v-divider />

    <!-- Transcript segments -->
    <div ref="listRef" class="flex-grow-1 overflow-y-auto pa-3">
      <div
        v-if="segments.length === 0"
        class="text-center text-medium-emphasis mt-4"
      >
        {{ $t('call.noTranscript') }}
      </div>
      <div
        v-for="seg in segments"
        :key="seg.id"
        class="mb-3"
      >
        <div class="d-flex align-start">
          <v-avatar size="28" color="primary" class="mr-2 mt-1">
            <span class="text-caption">{{ seg.speaker_name.charAt(0).toUpperCase() }}</span>
          </v-avatar>
          <div class="flex-grow-1" style="min-width: 0;">
            <div class="d-flex align-center ga-2">
              <span class="text-subtitle-2 font-weight-bold text-truncate">
                {{ seg.speaker_name }}
              </span>
              <span class="text-caption text-medium-emphasis flex-shrink-0">
                {{ formatTimestamp(seg.start_time) }}
              </span>
              <v-chip
                v-if="seg.language"
                size="x-small"
                variant="outlined"
              >
                {{ seg.language }}
              </v-chip>
              <v-chip
                v-if="seg.inference_duration_ms"
                size="x-small"
                variant="tonal"
                color="grey"
              >
                {{ seg.inference_duration_ms }}ms
              </v-chip>
            </div>
            <div class="text-body-2" style="word-break: break-word;">{{ seg.text }}</div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, watch, nextTick } from 'vue'
import type { TranscriptSegment } from '@/stores/rooms'

const props = defineProps<{
  segments: TranscriptSegment[]
}>()

const listRef = ref<HTMLElement | null>(null)

function formatTimestamp(secs: number): string {
  const mins = Math.floor(secs / 60)
  const s = Math.floor(secs % 60)
  return `${mins}:${s.toString().padStart(2, '0')}`
}

function formatTimeSrt(secs: number): string {
  const h = Math.floor(secs / 3600)
  const m = Math.floor((secs % 3600) / 60)
  const s = Math.floor(secs % 60)
  const ms = Math.floor((secs % 1) * 1000)
  return `${h.toString().padStart(2, '0')}:${m.toString().padStart(2, '0')}:${s.toString().padStart(2, '0')},${ms.toString().padStart(3, '0')}`
}

function formatTimeVtt(secs: number): string {
  return formatTimeSrt(secs).replace(',', '.')
}

function downloadVtt() {
  let content = 'WEBVTT\n\n'
  props.segments.forEach((seg, i) => {
    content += `${i + 1}\n`
    content += `${formatTimeVtt(seg.start_time)} --> ${formatTimeVtt(seg.end_time)}\n`
    content += `<v ${seg.speaker_name}>${seg.text}\n\n`
  })
  downloadFile(content, 'transcript.vtt', 'text/vtt')
}

function downloadSrt() {
  let content = ''
  props.segments.forEach((seg, i) => {
    content += `${i + 1}\n`
    content += `${formatTimeSrt(seg.start_time)} --> ${formatTimeSrt(seg.end_time)}\n`
    content += `${seg.speaker_name}: ${seg.text}\n\n`
  })
  downloadFile(content, 'transcript.srt', 'text/srt')
}

function downloadFile(content: string, filename: string, mime: string) {
  const blob = new Blob([content], { type: mime })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  a.click()
  URL.revokeObjectURL(url)
}

// Auto-scroll when new segments arrive
watch(
  () => props.segments.length,
  async () => {
    await nextTick()
    if (listRef.value) {
      listRef.value.scrollTop = listRef.value.scrollHeight
    }
  },
)
</script>

<style scoped>
.transcript-panel {
  width: 320px;
  min-width: 280px;
  height: 100%;
  max-height: 100%;
  border-left: 1px solid rgba(var(--v-border-color), var(--v-border-opacity));
  background: rgb(var(--v-theme-surface));
}
</style>
