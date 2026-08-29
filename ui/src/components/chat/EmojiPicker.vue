<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-menu v-model="open" :close-on-content-click="false" location="top">
    <template #activator="{ props: menuProps }">
      <span v-bind="menuProps" />
    </template>
    <v-card>
      <div ref="pickerRef" />
    </v-card>
  </v-menu>
</template>

<script setup lang="ts">
import { ref, watch, nextTick, onBeforeUnmount, computed } from 'vue'
import { useTheme } from 'vuetify'
import data from '@emoji-mart/data'

const props = defineProps<{
  modelValue: boolean
}>()

const emit = defineEmits<{
  'update:modelValue': [value: boolean]
  select: [emoji: string]
}>()

const theme = useTheme()
const pickerRef = ref<HTMLElement | null>(null)
let pickerInstance: { remove?: () => void } | null = null

const open = computed({
  get: () => props.modelValue,
  set: (val) => emit('update:modelValue', val),
})

const isDark = computed(() => theme.global.current.value.dark)

watch(open, async (val) => {
  if (val) {
    await nextTick()
    if (pickerRef.value && !pickerInstance) {
      const { Picker } = await import('emoji-mart')
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const picker = new (Picker as any)({
        data,
        onEmojiSelect: (emoji: { native: string }) => {
          emit('select', emoji.native)
        },
        theme: isDark.value ? 'dark' : 'light',
        set: 'native',
        previewPosition: 'none',
        skinTonePosition: 'search',
      })
      pickerRef.value.appendChild(picker as unknown as Node)
      pickerInstance = { remove: () => (picker as unknown as HTMLElement).remove() }
    }
  } else {
    if (pickerInstance) {
      pickerInstance.remove?.()
      pickerInstance = null
    }
  }
})

onBeforeUnmount(() => {
  pickerInstance?.remove?.()
  pickerInstance = null
})
</script>
