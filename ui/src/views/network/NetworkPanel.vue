<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <h1 class="text-h5 text-md-h4 mb-2 mb-md-4">{{ $t('nav.network') }}</h1>

    <!-- Sections are real child routes navigated from the app drawer's
         Network group — the page-local v-list duplicated that nav 1:1 and
         is gone (2026-08-04). Full-width content; deep links unchanged. -->
    <router-view />
  </v-container>
</template>
