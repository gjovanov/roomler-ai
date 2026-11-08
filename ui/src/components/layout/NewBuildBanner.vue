<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!--
    #3582 — a new build is out, and reloading now would lose something: a
    remote session, a call, typed text, an open dialog. Nothing is blocked and
    nothing waits on a click: the page reloads by itself at the next quiet
    moment (utils/staleBuildWatch.ts). This offers it now, and says what Reload
    does to what is live. Rendered by App.vue inside a fullscreen viewer too,
    like the app's snackbar.
  -->
  <v-snackbar
    :model-value="banner.visible"
    :attach="attach"
    location="top"
    :timeout="-1"
    data-testid="new-build-banner"
  >
    <div class="d-flex align-center ga-3">
      <v-icon icon="mdi-update" />
      <div>
        <div>{{ t('newBuild.ready') }}</div>
        <div v-if="banner.hint" class="text-caption" data-testid="new-build-hint">
          {{ t(`newBuild.hint.${banner.hint}`) }}
        </div>
      </div>
    </div>
    <template #actions>
      <v-btn variant="text" data-testid="new-build-later" @click="dismissNewBuildBanner()">
        {{ t('newBuild.later') }}
      </v-btn>
      <v-btn variant="flat" color="primary" data-testid="new-build-reload" @click="reloadForNewBuild()">
        {{ t('newBuild.reload') }}
      </v-btn>
    </template>
  </v-snackbar>
</template>

<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import { dismissNewBuildBanner, newBuildBanner as banner, reloadForNewBuild } from '@/utils/staleBuildWatch'

defineProps<{
  /** The fullscreen element while one is up (App.vue's `fullscreenEl`). */
  attach?: HTMLElement
}>()

const { t } = useI18n()
</script>
