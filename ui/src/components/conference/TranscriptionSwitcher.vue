<template>
  <v-menu :close-on-content-click="false" location="bottom end">
    <template #activator="{ props: menuProps }">
      <v-btn
        :icon="enabled ? 'mdi-subtitles' : 'mdi-subtitles-outline'"
        variant="text"
        :color="enabled ? 'primary' : undefined"
        v-bind="menuProps"
      />
    </template>

    <v-card min-width="260" class="pa-3">
      <v-card-subtitle class="px-0 pb-2">{{ $t('call.transcriptionModel') }}</v-card-subtitle>

      <!-- Model selection -->
      <v-radio-group
        :model-value="selectedModel"
        @update:model-value="$emit('update:model', $event)"
        density="compact"
        hide-details
      >
        <v-radio value="whisper" density="compact">
          <template #label>{{ $t('call.modelWhisper') }}</template>
        </v-radio>
        <v-radio value="canary" density="compact">
          <template #label>{{ $t('call.modelCanary') }}</template>
        </v-radio>
      </v-radio-group>

      <v-divider class="my-2" />

      <!-- Enable/disable toggle -->
      <v-switch
        :model-value="enabled"
        @update:model-value="$emit('toggle', $event)"
        :label="enabled ? $t('call.transcriptionOn') : $t('call.transcriptionOff')"
        density="compact"
        hide-details
        color="primary"
      />
    </v-card>
  </v-menu>
</template>

<script setup lang="ts">
defineProps<{
  enabled: boolean
  selectedModel: 'whisper' | 'canary'
}>()

defineEmits<{
  'update:model': [model: 'whisper' | 'canary']
  toggle: [enabled: boolean]
}>()
</script>
