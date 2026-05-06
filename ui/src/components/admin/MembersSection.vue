<template>
  <v-card>
    <v-card-title>Members</v-card-title>
    <v-card-text>
      <v-list>
        <v-list-item
          prepend-icon="mdi-account"
          title="Member management coming soon"
          subtitle="Invite, remove, and manage member roles"
        />
      </v-list>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
defineProps<{ tenantId: string }>()
</script>
