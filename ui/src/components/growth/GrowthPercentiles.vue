<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- FR-102 — a time-to-value measure: median and p75 in hours or days,
       always beside the n they rest on. `null` is "not measured". -->
  <span v-if="!p">
    <growth-not-measured reason="not mounted" />
  </span>
  <span v-else-if="p.n === 0" class="text-medium-emphasis text-body-2">no settled cohorts yet</span>
  <span v-else>
    <span class="text-h6">{{ human(p.median_h) }}</span>
    <span class="text-caption text-medium-emphasis ml-1">
      median · p75 {{ human(p.p75_h) }} · n {{ p.n }}
    </span>
  </span>
</template>

<script setup lang="ts">
import type { Percentiles } from '@/stores/growth'
import GrowthNotMeasured from './GrowthNotMeasured.vue'

defineProps<{ p: Percentiles | null }>()

/** Hours under a day; days to one decimal above it. */
function human(h: number | null): string {
  if (h == null) return '—'
  if (h < 48) return `${Math.round(h * 10) / 10}h`
  return `${Math.round((h / 24) * 10) / 10}d`
}
</script>
