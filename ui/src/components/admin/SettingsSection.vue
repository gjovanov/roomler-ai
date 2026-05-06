<template>
  <v-card>
    <v-card-title>Workspace Settings</v-card-title>
    <v-card-text>
      <v-text-field
        :model-value="tenantStore.current?.name"
        label="Workspace Name"
        disabled
      />
      <v-text-field
        :model-value="tenantStore.current?.slug"
        label="Slug"
        disabled
      />
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { useTenantStore } from '@/stores/tenant'

defineProps<{ tenantId: string }>()
const tenantStore = useTenantStore()
</script>
