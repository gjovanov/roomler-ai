<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!--
  FR-90 P1f — what a tool call asked to do, by its tool (`utils/toolView.ts`):
  a command line, an edit as a diff, a new file's first lines, a to-do list,
  a file and its lines, a search. For a call in the transcript and for the
  approval a driver answers. Every string is the model's and goes in by text
  interpolation, never as markup; a URL is shown as text, not followed.
-->
<template>
  <div class="hive-tool-input" :data-view="view.kind">
    <template v-if="view.kind === 'bash'">
      <div v-if="view.description" class="text-caption text-medium-emphasis">{{ view.description }}</div>
      <pre class="hive-code" data-testid="hive-command">$ {{ view.command }}</pre>
      <div v-if="view.background" class="text-caption text-medium-emphasis">{{ $t('hive.viewer.tool.background') }}</div>
    </template>

    <template v-else-if="view.kind === 'edit'">
      <div class="hive-path">{{ view.path }}</div>
      <div v-for="(c, i) in view.changes" :key="i" class="mt-1">
        <div v-if="c.all" class="text-caption text-medium-emphasis">{{ $t('hive.viewer.tool.replaceAll') }}</div>
        <div v-if="c.rows" class="hive-diff" data-testid="hive-diff">
          <div v-for="(r, j) in c.rows" :key="j" class="hive-diff-row" :class="`hive-diff-${r.kind}`" :data-kind="r.kind"><template v-if="r.kind === 'skip'">⋯ {{ $t('hive.viewer.tool.unchanged', { count: r.count }) }}</template><template v-else><span class="hive-diff-mark">{{ mark(r.kind) }}</span>{{ r.text }}</template></div>
        </div>
        <template v-else>
          <div class="text-caption text-medium-emphasis">{{ $t('hive.viewer.tool.tooLong') }}</div>
          <details>
            <summary class="text-caption">{{ $t('hive.viewer.tool.before') }}</summary>
            <pre class="hive-code">{{ c.before }}</pre>
          </details>
          <details>
            <summary class="text-caption">{{ $t('hive.viewer.tool.after') }}</summary>
            <pre class="hive-code">{{ c.after }}</pre>
          </details>
        </template>
      </div>
    </template>

    <template v-else-if="view.kind === 'write'">
      <div class="hive-path">{{ view.path }}</div>
      <div class="text-caption text-medium-emphasis">{{ $t('hive.viewer.tool.writes', { count: view.total }) }}</div>
      <div class="hive-diff" data-testid="hive-diff">
        <div v-for="(l, j) in view.head" :key="j" class="hive-diff-row hive-diff-add" data-kind="add"><span class="hive-diff-mark">+</span>{{ l }}</div>
        <div v-if="view.total > view.head.length" class="hive-diff-row hive-diff-skip" data-kind="skip">⋯ {{ $t('hive.viewer.tool.moreLines', { count: view.total - view.head.length }) }}</div>
      </div>
    </template>

    <div v-else-if="view.kind === 'read'" class="hive-path">
      {{ view.path }}<span v-if="view.from" class="text-medium-emphasis"> · {{ view.to ? $t('hive.viewer.tool.lines', { from: view.from, to: view.to }) : $t('hive.viewer.tool.fromLine', { from: view.from }) }}</span>
    </div>

    <div v-else-if="view.kind === 'search'">
      <code class="hive-inline">{{ view.pattern }}</code><span v-if="view.where" class="text-caption text-medium-emphasis"> · {{ view.where }}</span>
    </div>

    <ul v-else-if="view.kind === 'todos'" class="hive-todos">
      <li v-for="(t, i) in view.items" :key="i" :data-status="t.status">
        <v-icon size="x-small" :icon="todoIcon(t.status)" :aria-label="$t(`hive.viewer.tool.todo.${t.status}`)" />
        <span :class="{ 'hive-todo-done': t.status === 'completed', 'font-weight-medium': t.status === 'in_progress' }">{{ t.text }}</span>
      </li>
    </ul>

    <template v-else-if="view.kind === 'fetch'">
      <div class="hive-path">{{ view.url }}</div>
      <div v-if="view.prompt" class="text-caption text-medium-emphasis">{{ view.prompt }}</div>
    </template>

    <div v-else-if="view.kind === 'query'"><code class="hive-inline">{{ view.query }}</code></div>

    <template v-else-if="view.kind === 'task'">
      <div v-if="view.description" class="text-body-2">{{ view.description }}</div>
      <div v-if="view.agent" class="text-caption text-medium-emphasis">{{ $t('hive.viewer.tool.subagent', { agent: view.agent }) }}</div>
      <details v-if="view.prompt">
        <summary class="text-caption">{{ $t('hive.viewer.tool.prompt') }}</summary>
        <pre class="hive-code">{{ view.prompt }}</pre>
      </details>
    </template>

    <pre v-else class="hive-code">{{ view.text }}</pre>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { toolView, type TodoStatus } from '@/utils/toolView'

const props = defineProps<{ name: string; input: unknown }>()
const view = computed(() => toolView(props.name, props.input))

function mark(kind: 'same' | 'add' | 'del'): string {
  return kind === 'add' ? '+' : kind === 'del' ? '−' : ' '
}

function todoIcon(s: TodoStatus): string {
  return s === 'completed' ? 'mdi-checkbox-marked-outline' : s === 'in_progress' ? 'mdi-progress-clock' : 'mdi-checkbox-blank-outline'
}
</script>

<style scoped>
.hive-code {
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
.hive-path {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 0.8rem;
  word-break: break-all;
}
.hive-inline {
  font-size: 0.8rem;
  padding: 0 0.25rem;
  border-radius: 3px;
  background: rgba(var(--v-theme-on-surface), 0.06);
}
.hive-diff {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 0.8rem;
  max-height: 24rem;
  overflow: auto;
  border-radius: 4px;
  background: rgba(var(--v-theme-on-surface), 0.04);
}
.hive-diff-row {
  white-space: pre-wrap;
  word-break: break-word;
  padding: 0 0.5rem;
}
.hive-diff-mark {
  display: inline-block;
  width: 1.25em;
  user-select: none;
  opacity: 0.7;
}
.hive-diff-add {
  background: rgba(var(--v-theme-success), 0.14);
}
.hive-diff-del {
  background: rgba(var(--v-theme-error), 0.12);
}
.hive-diff-skip {
  font-style: italic;
  opacity: 0.6;
}
.hive-todos {
  list-style: none;
  padding: 0;
  margin: 0;
}
.hive-todos li {
  display: flex;
  align-items: baseline;
  gap: 0.4rem;
  font-size: 0.875rem;
}
.hive-todo-done {
  text-decoration: line-through;
  opacity: 0.6;
}
</style>
