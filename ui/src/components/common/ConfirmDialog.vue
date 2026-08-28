<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-dialog v-model="model" max-width="400">
    <v-card>
      <v-card-title>{{ title }}</v-card-title>
      <v-card-text>{{ message }}</v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn @click="model = false">{{ $t('common.cancel') }}</v-btn>
        <v-btn :color="confirmColor" @click="handleConfirm">{{ $t('common.confirm') }}</v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
const model = defineModel<boolean>({ required: true })

defineProps<{
  title: string
  message: string
  confirmColor?: string
}>()

const emit = defineEmits<{
  confirm: []
}>()

function handleConfirm() {
  emit('confirm')
  model.value = false
}
</script>
