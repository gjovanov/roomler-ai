<template>
  <v-card>
    <v-card-title>Roles &amp; Permissions</v-card-title>
    <v-card-text>
      <v-list>
        <v-list-item
          prepend-icon="mdi-shield-account"
          title="Role management coming soon"
          subtitle="Create, edit, and assign roles with permission bitfields"
        />
      </v-list>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
defineProps<{ tenantId: string }>()
</script>
