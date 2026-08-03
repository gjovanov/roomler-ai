<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <h1 class="text-h5 text-md-h4 mb-2 mb-md-4">{{ $t('nav.admin') }}</h1>

    <!-- Sections are real child routes (admin-settings, admin-members,
         admin-roles) navigated from the app drawer's Admin group — the
         page-local v-list duplicated that nav 1:1 and is gone (2026-08-04).
         Full-width content; the URL still reflects the active section. -->
    <router-view />
  </v-container>
</template>
