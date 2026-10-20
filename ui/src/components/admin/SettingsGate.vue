<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!-- FR-116 — one organization setting (a "gate") on the Settings page.

     From 600 px up it renders exactly the card each gate always was: a title
     (with its icon), then the body. On a phone the page was ~4 screens of
     warnings and captions before its last switch, so there each gate is an
     expansion panel whose title carries the gate's current state (`summary`):
     the page reads as a list of decisions, and a gate's explanation and
     controls open on demand.

     On `xs` it must sit inside a `v-expansion-panels` — SettingsSection's root
     is one there. Attributes (`data-testid`) land on the card or the panel. -->
<template>
  <v-expansion-panel v-if="xs" v-bind="$attrs">
    <v-expansion-panel-title>
      <div class="d-flex align-center ga-2 w-100" style="min-width: 0">
        <v-icon v-if="icon" :icon="icon" color="primary" size="small" />
        <span class="font-weight-medium" style="min-width: 0">{{ title }}</span>
        <v-spacer />
        <span
          v-if="summary"
          class="text-caption text-medium-emphasis text-right mr-2"
          data-testid="settings-gate-summary"
        >{{ summary }}</span>
      </div>
    </v-expansion-panel-title>
    <v-expansion-panel-text>
      <slot />
    </v-expansion-panel-text>
  </v-expansion-panel>
  <v-card v-else v-bind="$attrs">
    <v-card-title v-if="icon" class="d-flex align-center">
      <v-icon :icon="icon" color="primary" class="mr-2" />
      {{ title }}
    </v-card-title>
    <v-card-title v-else>{{ title }}</v-card-title>
    <v-card-text>
      <slot />
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { useDisplay } from 'vuetify'

defineOptions({ inheritAttrs: false })
defineProps<{
  title: string
  icon?: string
  /** The gate's current state, for the collapsed panel on a phone ("On",
   *  "Off", "1 hour max"). Empty while it is unknown — not loaded, or not
   *  readable with the caller's permissions. */
  summary?: string
}>()

const { xs } = useDisplay()
</script>
