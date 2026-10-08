<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!--
  FR-90 P1f — what a tool call returned, as much as its tool calls for
  (`resultMode`): in full, folded behind a line count, as one line, or not at
  all. A failure always shows in full. Under its call, or on its own after
  the approval that let it run. Output is coloured as a terminal would show
  it (`HiveAnsi`), and stays text.
-->
<template>
  <div v-if="mode !== 'hidden'" class="hive-tool-result" data-testid="hive-tool-result">
    <hive-ansi v-if="mode === 'output'" :text="result.output" :error="!result.ok" />
    <details v-else-if="mode === 'folded'" data-testid="hive-tool-folded">
      <summary class="text-caption text-medium-emphasis">{{ $t('hive.viewer.tool.output', { count: lineCount }) }}</summary>
      <hive-ansi :text="result.output" />
    </details>
    <div v-else-if="mode === 'line'" class="text-caption text-medium-emphasis" data-testid="hive-tool-line">
      <v-icon size="x-small" color="success" icon="mdi-check" /> {{ firstLine }}
    </div>
    <div v-if="result.truncated" class="text-caption text-medium-emphasis">{{ $t('hive.viewer.truncated') }}</div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import HiveAnsi from '@/components/hive/HiveAnsi.vue'
import { resultMode } from '@/utils/toolView'

const props = defineProps<{
  /** The tool that ran; empty when its call is not known (in full, then). */
  name: string
  result: { ok: boolean; output: string; truncated?: boolean }
}>()

const mode = computed(() => resultMode(props.name, props.result.ok))
const lineCount = computed(() => {
  const out = props.result.output
  return out === '' ? 0 : out.replace(/\n$/, '').split('\n').length
})
const firstLine = computed(() => props.result.output.split('\n', 1)[0])
</script>
