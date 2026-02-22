<template>
  <v-menu :close-on-content-click="false" location="bottom end">
    <template #activator="{ props: menuProps }">
      <v-btn icon="mdi-view-grid" variant="text" v-bind="menuProps" />
    </template>

    <v-card min-width="280" class="pa-3">
      <v-card-subtitle class="px-0 pb-2">{{ $t('call.layout') }}</v-card-subtitle>

      <!-- Layout mode -->
      <v-radio-group
        :model-value="prefs.mode"
        @update:model-value="$emit('update:mode', $event)"
        density="compact"
        hide-details
      >
        <v-radio value="auto" density="compact">
          <template #label>
            <v-icon size="18" class="mr-1">mdi-auto-fix</v-icon>
            {{ $t('call.layoutAuto') }}
          </template>
        </v-radio>
        <v-radio value="tiled" density="compact">
          <template #label>
            <v-icon size="18" class="mr-1">mdi-view-grid</v-icon>
            {{ $t('call.layoutTiled') }}
          </template>
        </v-radio>
        <v-radio value="spotlight" density="compact">
          <template #label>
            <v-icon size="18" class="mr-1">mdi-spotlight-beam</v-icon>
            {{ $t('call.layoutSpotlight') }}
          </template>
        </v-radio>
        <v-radio value="sidebar" density="compact">
          <template #label>
            <v-icon size="18" class="mr-1">mdi-page-layout-sidebar-right</v-icon>
            {{ $t('call.layoutSidebar') }}
          </template>
        </v-radio>
      </v-radio-group>

      <v-divider class="my-2" />

      <!-- Tile count slider (shown for tiled/auto modes) -->
      <div v-if="prefs.mode === 'tiled' || prefs.mode === 'auto'">
        <div class="text-caption text-medium-emphasis mb-1">
          {{ $t('call.tileCount') }}: {{ prefs.tiledMaxTiles }}
        </div>
        <v-slider
          :model-value="prefs.tiledMaxTiles"
          @update:model-value="$emit('update:maxTiles', $event)"
          :min="4"
          :max="49"
          :step="1"
          density="compact"
          hide-details
          thumb-label
        />
      </div>

      <v-divider class="my-2" />

      <!-- Hide non-video -->
      <v-switch
        :model-value="prefs.hideNonVideo"
        @update:model-value="$emit('update:hideNonVideo', $event)"
        :label="$t('call.hideNonVideo')"
        density="compact"
        hide-details
        color="primary"
      />

      <v-divider class="my-2" />

      <!-- Self-view mode -->
      <v-card-subtitle class="px-0 pb-1">{{ $t('call.selfView') }}</v-card-subtitle>
      <v-radio-group
        :model-value="prefs.selfViewMode"
        @update:model-value="$emit('update:selfViewMode', $event)"
        density="compact"
        hide-details
      >
        <v-radio value="in-grid-cropped" density="compact">
          <template #label>{{ $t('call.selfViewCropped') }}</template>
        </v-radio>
        <v-radio value="in-grid-uncropped" density="compact">
          <template #label>{{ $t('call.selfViewUncropped') }}</template>
        </v-radio>
        <v-radio value="floating-uncropped" density="compact">
          <template #label>{{ $t('call.selfViewFloating') }}</template>
        </v-radio>
      </v-radio-group>
    </v-card>
  </v-menu>
</template>

<script setup lang="ts">
import type { LayoutPreferences, LayoutMode, SelfViewMode } from '@/composables/useConferenceLayout'

defineProps<{
  prefs: LayoutPreferences
}>()

defineEmits<{
  'update:mode': [mode: LayoutMode]
  'update:maxTiles': [n: number]
  'update:hideNonVideo': [v: boolean]
  'update:selfViewMode': [mode: SelfViewMode]
}>()
</script>
