<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!--
  FR-39 — the fallback for a visitor who is interested but not ready to create
  an account. Without it, everyone who does not convert on the spot is
  unreachable forever, and every burst of traffic is a spike rather than an
  audience.

  The server answers 202 for every outcome — new address, known address,
  previously unsubscribed — so this component deliberately shows ONE message and
  cannot report "you are already subscribed". That is the point: telling them
  apart here would leak list membership for an address the visitor may not own.
-->
<template>
  <div class="stay-in-touch">
    <p class="text-body-2 mb-3 stay-sub">
      Not ready to sign up? Get an email when something notable ships.
    </p>

    <v-form v-if="!done" class="d-flex flex-wrap ga-2 justify-center" @submit.prevent="submit">
      <v-text-field
        v-model="email"
        type="email"
        name="email"
        autocomplete="email"
        placeholder="you@example.com"
        density="compact"
        variant="solo"
        hide-details
        single-line
        :disabled="busy"
        class="stay-field"
        aria-label="Your email address"
      />
      <v-btn
        type="submit"
        color="white"
        variant="flat"
        class="text-primary"
        :loading="busy"
        :disabled="!email.trim()"
      >
        Keep me posted
      </v-btn>
    </v-form>

    <p v-else class="text-body-2 stay-done" role="status">
      Thanks — check your inbox for a confirmation link.
    </p>

    <p class="text-caption mt-3 stay-fine">
      Product updates only, and never more than monthly. One-click unsubscribe in
      every email. We do not share your address.
      <router-link to="/privacy" class="stay-link">Privacy&nbsp;Policy</router-link>
    </p>
  </div>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import { api } from '@/api/client'
import { useSnackbar } from '@/composables/useSnackbar'

const props = withDefaults(defineProps<{ source?: string }>(), { source: 'landing' })

const email = ref('')
const busy = ref(false)
const done = ref(false)
const { showError } = useSnackbar()

async function submit() {
  if (busy.value || !email.value.trim()) return
  busy.value = true
  try {
    await api.post('/api/subscribe', { email: email.value.trim(), source: props.source })
    // Success is the ONLY branch, by design — see the comment at the top of
    // this file. The server does not tell us which outcome occurred.
    done.value = true
    email.value = ''
  } catch {
    // Only a transport or server failure lands here; a rejected address does
    // not, because the endpoint accepts everything that parses.
    showError('Could not reach the server. Please try again.')
  } finally {
    busy.value = false
  }
}
</script>

<style scoped>
.stay-in-touch {
  max-width: 520px;
  margin: 0 auto;
}
.stay-field {
  flex: 1 1 240px;
  min-width: 0;
}
.stay-sub,
.stay-fine,
.stay-done {
  color: rgba(255, 255, 255, 0.86);
}
.stay-fine {
  color: rgba(255, 255, 255, 0.68);
  line-height: 1.5;
}
.stay-link {
  color: rgba(255, 255, 255, 0.92);
  text-underline-offset: 2px;
}
</style>
