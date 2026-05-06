<template>
  <v-card>
    <v-card-title>Audit Log</v-card-title>
    <v-card-text>
      <v-list>
        <v-list-item
          prepend-icon="mdi-clipboard-text-clock"
          title="Audit log coming soon"
          subtitle="Track all administrative actions"
        />
      </v-list>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
defineProps<{ tenantId: string }>()
</script>
