<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!--
  #3029 — where the activation email's link lands:
  `/auth/activate?userId=…&token=…` (built by `crates/api/src/routes/auth.rs`).

  It redeems the token on arrival (`utils/activation.ts`: one POST per link,
  a refresh shows the outcome again) and then hands the person to sign-in.
  No `meta.auth` and no `meta.guest`: there is no session yet, and a
  signed-in browser may still be activating another account's link.

  #3199 — a link that is refused offers a new one, and so does this page with
  no link at all, which is how sign-in and sign-up reach it. The answer is one
  sentence whatever the address, because the server's is. Registering again is
  no way back: the unverified row still holds the address, so it answers 409.

  A raw fetch, not the api client, as the newsletter confirm page does: an
  anonymous visitor must never be routed through the 401-refresh-logout
  chain, and a 5xx here is this page's to explain, not a snackbar's.
-->
<template>
  <v-container class="fill-height pa-2 pa-md-4 pa-xl-6 page-no-sideways" fluid>
    <v-row align="center" justify="center">
      <v-col cols="12" sm="8" md="6" class="text-center">
        <template v-if="phase === 'working'">
          <v-progress-circular indeterminate color="primary" size="64" class="mb-6" />
          <p class="text-body-1 text-medium-emphasis" data-test="activate-working">{{ $t('activation.working') }}</p>
        </template>

        <template v-else-if="phase === 'done'">
          <v-icon icon="mdi-account-check-outline" color="success" size="72" class="mb-6" />
          <h1 class="text-h5 text-md-h4 font-weight-bold mb-4" data-test="activate-done">{{ $t('activation.doneTitle') }}</h1>
          <p class="text-body-1 text-medium-emphasis mb-8">{{ $t('activation.doneText') }}</p>
          <v-btn color="primary" size="large" to="/login" data-test="activate-sign-in">{{ $t('activation.signIn') }}</v-btn>
        </template>

        <template v-else-if="phase === 'invalid' || phase === 'ask'">
          <v-icon :icon="phase === 'invalid' ? 'mdi-link-off' : 'mdi-email-outline'" color="warning" size="72" class="mb-6" />
          <h1 v-if="phase === 'invalid'" class="text-h5 text-md-h4 font-weight-bold mb-4" data-test="activate-invalid">{{ $t('activation.invalidTitle') }}</h1>
          <h1 v-else class="text-h5 text-md-h4 font-weight-bold mb-4" data-test="activate-ask">{{ $t('activation.askTitle') }}</h1>
          <p class="text-body-1 text-medium-emphasis mb-6">{{ $t(phase === 'invalid' ? 'activation.invalidText' : 'activation.askText') }}</p>
          <v-form data-test="activate-resend" @submit.prevent="resend">
            <v-text-field
              v-model="email"
              :label="$t('activation.email')"
              type="email"
              prepend-inner-icon="mdi-email"
              autocomplete="email"
              autocapitalize="none"
              autocorrect="off"
              spellcheck="false"
              data-test="activate-email"
            />
            <v-alert
              v-if="resendOutcome"
              :type="resendOutcome === 'sent' ? 'success' : 'warning'"
              variant="tonal"
              density="compact"
              class="mb-4 text-left"
              data-test="activate-resend-outcome"
            >
              {{ $t(`activation.resend.${resendOutcome}`) }}
            </v-alert>
            <div class="d-flex flex-column flex-sm-row justify-center ga-3">
              <v-btn
                type="submit"
                color="primary"
                size="large"
                :loading="resending"
                :disabled="!email.trim()"
                data-test="activate-resend-send"
              >
                {{ $t('activation.send') }}
              </v-btn>
              <v-btn variant="outlined" size="large" to="/login" data-test="activate-sign-in">{{ $t('activation.signIn') }}</v-btn>
            </div>
          </v-form>
        </template>

        <template v-else>
          <v-icon icon="mdi-wifi-off" color="warning" size="72" class="mb-6" />
          <h1 class="text-h5 text-md-h4 font-weight-bold mb-4" data-test="activate-error">{{ $t('activation.errorTitle') }}</h1>
          <p class="text-body-1 text-medium-emphasis mb-8">{{ $t('activation.errorText') }}</p>
          <v-btn color="primary" size="large" data-test="activate-retry" @click="run">{{ $t('activation.retry') }}</v-btn>
        </template>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import {
  activateAccount,
  activationParams,
  requestActivationLink,
  wasActivated,
  type ResendOutcome,
} from '@/utils/activation'

type Phase = 'working' | 'done' | 'invalid' | 'ask' | 'error'

const route = useRoute()
const phase = ref<Phase>('working')
const email = ref('')
const resending = ref(false)
const resendOutcome = ref<ResendOutcome | null>(null)

async function run() {
  const params = activationParams(route.query)
  // A link missing either half can only ever be refused: say so without
  // asking. No link at all is someone come from sign-in or sign-up to ask.
  if (!params) {
    phase.value = Object.keys(route.query).length > 0 ? 'invalid' : 'ask'
    return
  }
  // A refresh after success: the token is spent, the account is active.
  if (wasActivated(params.userId)) {
    phase.value = 'done'
    return
  }
  phase.value = 'working'
  const outcome = await activateAccount(params)
  phase.value = outcome === 'ok' ? 'done' : outcome
}

async function resend() {
  const address = email.value.trim()
  if (!address || resending.value) return
  resending.value = true
  resendOutcome.value = await requestActivationLink(address)
  resending.value = false
}

onMounted(run)
</script>
