<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-dialog :model-value="modelValue" max-width="440" @update:model-value="emit('update:modelValue', $event)">
    <v-card>
      <v-toolbar color="surface" flat density="compact">
        <v-toolbar-title class="text-body-1 font-weight-bold">{{ $t('grid.columns') }}</v-toolbar-title>
        <v-spacer />
        <v-btn icon="mdi-close" variant="text" size="small" @click="emit('update:modelValue', false)" />
      </v-toolbar>
      <v-card-text class="pa-2">
        <!-- Reorder by drag & drop (SortableJS handles touch via long-press).
             The checkbox toggles visibility; the whole row drags by its handle.
             Ported from lgr's ui-shared. -->
        <draggable
          v-model="localEntries"
          item-key="key"
          handle=".drag-handle"
          :animation="150"
          :delay="150"
          :delay-on-touch-only="true"
          ghost-class="drag-ghost"
          tag="div"
          class="v-list v-list--density-compact"
          @end="onDragEnd"
        >
          <template #item="{ element: col }">
            <v-list-item class="px-2">
              <template #prepend>
                <v-icon class="drag-handle mr-1" size="small" color="medium-emphasis" style="cursor: grab">
                  mdi-drag-vertical
                </v-icon>
                <v-checkbox-btn
                  :model-value="col.visible"
                  :disabled="col.locked"
                  density="compact"
                  @update:model-value="emit('toggle', col.key)"
                />
              </template>
              <v-list-item-title :class="col.visible ? '' : 'text-medium-emphasis'">{{ col.title || col.key }}</v-list-item-title>
            </v-list-item>
          </template>
        </draggable>
        <!-- Host-provided extras (e.g. grid-specific display toggles). -->
        <slot name="append" />
      </v-card-text>
      <v-card-actions class="px-4 py-2">
        <v-btn variant="text" prepend-icon="mdi-restore" @click="emit('reset')">{{ $t('grid.reset') }}</v-btn>
        <v-spacer />
        <v-btn color="primary" variant="flat" @click="emit('update:modelValue', false)">{{ $t('common.close') }}</v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { ref, watch } from 'vue'
import draggable from 'vuedraggable'
import type { GridColumnEntry } from '@/composables/useGridColumns'

const props = defineProps<{
  modelValue: boolean
  entries: GridColumnEntry[]
}>()

const emit = defineEmits<{
  'update:modelValue': [value: boolean]
  toggle: [key: string]
  /** Full key order after a drag — the composable persists it in one write. */
  reorder: [keys: string[]]
  reset: []
}>()

// Local copy: draggable mutates its list in place; the source of truth stays
// in the composable, which echoes back through the entries prop after
// `reorder` persists.
const localEntries = ref<GridColumnEntry[]>([])
watch(
  () => props.entries,
  (v) => {
    localEntries.value = [...v]
  },
  { immediate: true },
)

function onDragEnd() {
  emit(
    'reorder',
    localEntries.value.map((e) => e.key),
  )
}
</script>

<style scoped>
.drag-ghost {
  opacity: 0.4;
}
</style>
