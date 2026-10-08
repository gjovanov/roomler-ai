<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!--
  FR-90 P1f — one tool call in a session's transcript: its tool, what it
  asked to do (`HiveToolInput`), and under it what it returned
  (`HiveToolResult`). A call that waited for an approval is drawn by its
  approval card instead, and its result after the answer.
-->
<template>
  <div class="hive-tool" :data-tool="name" data-testid="hive-tool">
    <div class="text-caption"><v-icon size="x-small" :icon="toolIcon(name)" /> {{ name }}</div>
    <hive-tool-input :name="name" :input="input" />
    <hive-tool-result v-if="result" class="mt-1" :name="name" :result="result" />
  </div>
</template>

<script setup lang="ts">
import HiveToolInput from '@/components/hive/HiveToolInput.vue'
import HiveToolResult from '@/components/hive/HiveToolResult.vue'
import { toolIcon } from '@/utils/toolView'

defineProps<{
  name: string
  input: unknown
  /** The call's `tool_result`, once there is one. */
  result?: { ok: boolean; output: string; truncated?: boolean } | null
}>()
</script>
