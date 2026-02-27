<template>
  <v-app>
    <router-view />
    <v-snackbar
      v-model="snackbar.state.show"
      :color="snackbar.state.color"
      :timeout="snackbar.state.timeout"
      location="bottom right"
    >
      {{ snackbar.state.text }}
      <template #actions>
        <v-btn variant="text" @click="snackbar.hideSnackbar()">Close</v-btn>
      </template>
    </v-snackbar>
  </v-app>
</template>

<script setup lang="ts">
import { useSnackbar } from '@/composables/useSnackbar'

const snackbar = useSnackbar()
</script>
