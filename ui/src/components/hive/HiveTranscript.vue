<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!--
  FR-90 P0d-3 — an agent session's transcript, read from the device over a
  viewer peer, with the "ask the agent" composer for a viewer that may drive.

  Assistant text is markdown through the SAME pipeline as chat messages
  (`renderMarkdown`: markdown-it, then DOMPurify — the one XSS boundary for
  message content). Everything else — prompts, tool inputs, tool output — is
  data rendered by text interpolation, never an HTML string.
-->
<template>
  <div class="hive-transcript d-flex flex-column">
    <div class="d-flex align-center px-3 py-2 ga-2">
      <v-chip size="small" :color="statusColor" variant="tonal" data-testid="hive-viewer-status">
        {{ $t(`hive.viewer.status.${viewer.status.value}`) }}
      </v-chip>
      <v-chip v-if="viewer.runState.value" size="small" variant="outlined">
        {{ $t(`hive.state.${viewer.runState.value}`, viewer.runState.value) }}
      </v-chip>
      <span v-if="viewer.reason.value" class="text-caption text-medium-emphasis text-truncate">
        {{ viewer.reason.value }}
      </span>
      <v-spacer />
      <v-btn
        v-if="viewer.status.value === 'closed' || viewer.status.value === 'refused'"
        size="small"
        variant="text"
        prepend-icon="mdi-refresh"
        @click="viewer.open(sessionId)"
      >
        {{ $t('hive.viewer.reconnect') }}
      </v-btn>
    </div>
    <v-divider />

    <div ref="listRef" class="flex-grow-1 overflow-y-auto pa-3" style="min-height: 0" @scroll="onScroll">
      <div v-if="viewer.hasEarlier.value" class="text-center mb-2">
        <v-btn size="small" variant="text" @click="viewer.loadEarlier()">{{ $t('hive.viewer.loadEarlier') }}</v-btn>
      </div>
      <div
        v-if="viewer.status.value === 'open' && viewer.events.value.length === 0"
        class="text-center text-medium-emphasis pa-6"
      >
        {{ $t('hive.viewer.empty') }}
      </div>
      <div v-for="e in shown" :key="e.seq" class="hive-event mb-2" :data-kind="e.event.kind">
        <template v-if="e.event.kind === 'user_message'">
          <div class="text-caption text-medium-emphasis">{{ asText(e.event.author) || $t('hive.viewer.someone') }}</div>
          <div class="hive-prompt pa-2 rounded">{{ asText(e.event.text) }}</div>
        </template>
        <!-- eslint-disable-next-line vue/no-v-html -- renderMarkdown is the sanitising boundary -->
        <div v-else-if="e.event.kind === 'assistant_text'" class="hive-assistant" v-html="renderMarkdown(asText(e.event.text))" />
        <details v-else-if="e.event.kind === 'thinking'" class="text-medium-emphasis">
          <summary class="text-caption">{{ $t('hive.viewer.thinking') }}</summary>
          <div class="hive-pre">{{ asText(e.event.summary) }}</div>
        </details>
        <div v-else-if="e.event.kind === 'tool_use'">
          <div class="text-caption"><v-icon size="x-small">mdi-wrench</v-icon> {{ asText(e.event.name) }}</div>
          <pre class="hive-pre">{{ pretty(e.event.input) }}</pre>
        </div>
        <div v-else-if="e.event.kind === 'tool_result'">
          <pre class="hive-pre" :class="{ 'hive-error': e.event.ok === false }">{{ asText(e.event.output) }}</pre>
          <div v-if="e.event.truncated" class="text-caption text-medium-emphasis">{{ $t('hive.viewer.truncated') }}</div>
        </div>
        <div v-else-if="e.event.kind === 'turn'" class="text-caption text-medium-emphasis hive-turn">
          {{ turnLine(e.event) }}
        </div>
        <div v-else-if="e.event.kind === 'session_init'" class="text-caption text-medium-emphasis">
          {{ $t('hive.viewer.started', { model: asText(e.event.model) || '—', cwd: asText(e.event.cwd) || '—' }) }}
        </div>
        <div v-else-if="e.event.kind === 'note'" class="text-caption text-medium-emphasis">{{ asText(e.event.text) }}</div>
        <div v-else-if="e.event.kind === 'compaction'" class="text-caption text-medium-emphasis">
          {{ $t('hive.viewer.compacted') }}
        </div>
        <div v-else class="text-caption text-medium-emphasis">
          {{ $t('hive.viewer.unknownKind', { kind: asText(e.event.kind) }) }}
        </div>
      </div>
    </div>

    <template v-if="viewer.mayPrompt.value && viewer.status.value === 'open'">
      <v-divider />
      <div class="pa-2 d-flex align-end ga-2">
        <v-textarea
          v-model="draft"
          :placeholder="$t('hive.viewer.askPlaceholder')"
          auto-grow
          rows="1"
          max-rows="8"
          hide-details
          density="compact"
          variant="outlined"
          data-testid="hive-ask"
          @keydown.enter.exact.prevent="ask"
        />
        <v-btn color="primary" :loading="asking" :disabled="!draft.trim()" icon="mdi-send" @click="ask" />
      </div>
      <div v-if="askError" class="px-3 pb-2 text-caption text-error">{{ askError }}</div>
    </template>
    <div
      v-else-if="viewer.status.value === 'open' && !viewer.mayPrompt.value"
      class="px-3 py-2 text-caption text-medium-emphasis"
    >
      {{ $t('hive.viewer.readOnly') }}
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { renderMarkdown } from '@/composables/useMarkdown'
import { useHiveViewer, withoutRepeatedInits } from '@/composables/useHiveViewer'

const props = defineProps<{ sessionId: string }>()
const { t } = useI18n()
const viewer = useHiveViewer()
const shown = computed(() => withoutRepeatedInits(viewer.events.value))

const listRef = ref<HTMLElement | null>(null)
const nearBottom = ref(true)
const draft = ref('')
const asking = ref(false)
const askError = ref<string | null>(null)

const statusColor = computed(() => {
  switch (viewer.status.value) {
    case 'open':
      return 'success'
    case 'opening':
    case 'connecting':
      return 'info'
    case 'refused':
      return 'error'
    default:
      return undefined
  }
})

function asText(v: unknown): string {
  return typeof v === 'string' ? v : v == null ? '' : String(v)
}

function pretty(v: unknown): string {
  try {
    return JSON.stringify(v, null, 2)
  } catch {
    return String(v)
  }
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function turnLine(e: any): string {
  const parts = [e.ok === false ? t('hive.viewer.turnError') : t('hive.viewer.turnDone')]
  if (typeof e.duration_ms === 'number') parts.push(`${Math.round(e.duration_ms / 1000)} s`)
  if (typeof e.cost_usd === 'number' && Number.isFinite(e.cost_usd)) parts.push(`$${e.cost_usd.toFixed(2)}`)
  return parts.join(' · ')
}

function onScroll(): void {
  const el = listRef.value
  if (!el) return
  nearBottom.value = el.scrollHeight - el.scrollTop - el.clientHeight < 80
}

watch(
  () => viewer.events.value.length,
  async () => {
    if (!nearBottom.value) return
    await nextTick()
    const el = listRef.value
    if (el) el.scrollTop = el.scrollHeight
  },
)

async function ask(): Promise<void> {
  const text = draft.value.trim()
  if (!text || asking.value) return
  asking.value = true
  askError.value = null
  const res = await viewer.prompt(text)
  asking.value = false
  if (res.ok) {
    draft.value = ''
    nearBottom.value = true
  } else {
    askError.value = res.error ?? t('hive.viewer.askFailed')
  }
}

onMounted(() => viewer.open(props.sessionId))
watch(
  () => props.sessionId,
  (id) => id && viewer.open(id),
)
</script>

<style scoped>
.hive-transcript {
  height: 100%;
  min-height: 0;
}
.hive-prompt {
  white-space: pre-wrap;
  word-break: break-word;
  background: rgba(var(--v-theme-primary), 0.08);
}
.hive-pre {
  white-space: pre-wrap;
  word-break: break-word;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 0.8rem;
  max-height: 24rem;
  overflow: auto;
  margin: 0;
  padding: 0.5rem;
  border-radius: 4px;
  background: rgba(var(--v-theme-on-surface), 0.05);
}
.hive-error {
  border-left: 3px solid rgb(var(--v-theme-error));
}
.hive-turn {
  border-top: 1px dashed rgba(var(--v-theme-on-surface), 0.2);
  padding-top: 0.25rem;
}
</style>
