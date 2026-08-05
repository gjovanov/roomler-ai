<template>
  <v-btn-toggle
    :model-value="modelValue"
    density="compact"
    variant="outlined"
    divided
    mandatory
    @update:model-value="(v: unknown) => v && emit('update:modelValue', v as StatsRange)"
  >
    <v-btn v-for="r in RANGES" :key="r.value" :value="r.value" size="small">{{ r.label }}</v-btn>
  </v-btn-toggle>
</template>

<script setup lang="ts">
export type StatsRange = '24h' | '7d' | '30d' | '1y'

const RANGES: Array<{ value: StatsRange; label: string }> = [
  { value: '24h', label: '24h' },
  { value: '7d', label: '7d' },
  { value: '30d', label: '30d' },
  { value: '1y', label: '1y' },
]

defineProps<{ modelValue: StatsRange }>()
const emit = defineEmits<{ (e: 'update:modelValue', v: StatsRange): void }>()
</script>
