<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- #1631 — a bare <router-view> reuses the mounted component when only a
       param changes, so /agent/A/remote → /agent/B/remote kept A's viewer
       (toolbar, status, and Connect dialling A) under B's URL. A view whose
       route names `meta.remountOn` is keyed by that param and remounts when
       it changes; every other route keeps an undefined key, i.e. the reuse
       it had before (see `plugins/routeViewKey.ts` for why not fullPath). -->
  <router-view v-slot="{ Component, route }">
    <component :is="Component" :key="routeViewKey(route)" />
  </router-view>
</template>

<script setup lang="ts">
import { routeViewKey } from '@/plugins/routeViewKey'
</script>
