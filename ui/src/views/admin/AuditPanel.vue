<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <h1 class="text-h5 text-md-h4 mb-2 mb-md-4">{{ $t('nav.audit') }}</h1>

    <!-- Same shell pattern as AdminPanel: sections are child routes driven
         from the app drawer's Audit group (2026-08-26 — the three audit
         sections moved OUT of the Settings page, where reviewing history
         was buried under org configuration). -->
    <router-view />
  </v-container>
</template>
