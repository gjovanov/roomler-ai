<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <h1 class="text-h5 text-md-h4 mb-2 mb-md-4">{{ $t('nav.devices') }}</h1>
    <agents-section :tenant-id="tenantId" />
  </v-container>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { useRoute } from 'vue-router'
import AgentsSection from '@/components/admin/AgentsSection.vue'

const route = useRoute()
const tenantId = computed(() => route.params.tenantId as string)
</script>
