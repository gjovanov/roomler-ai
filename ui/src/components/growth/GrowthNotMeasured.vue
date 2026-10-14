<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- FR-102 — a "not measured" cell: a muted chip with its reason, never a
       0. The reason is server- or taxonomy-chosen text, rendered as text. -->
  <v-chip size="small" variant="tonal" color="grey" class="text-medium-emphasis">
    {{ reason.startsWith('not measured') ? reason : `not measured — ${reason}` }}
  </v-chip>
</template>

<script setup lang="ts">
defineProps<{ reason: string }>()
</script>
