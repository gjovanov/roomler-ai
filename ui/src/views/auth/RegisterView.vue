<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-container class="fill-height pa-2 pa-md-4 pa-xl-6 page-no-sideways" fluid>
    <v-row align="center" justify="center">
      <v-col cols="12" sm="8" md="4">
        <v-card class="pa-3 pa-md-4">
          <v-card-title class="text-center text-h5 mb-4">
            <img :src="brandMark" alt="" width="28" height="28" class="mr-2" style="vertical-align: middle" />
            {{ $t('auth.register') }}
          </v-card-title>

          <v-form ref="formRef" @submit.prevent="handleRegister">
            <v-text-field
              v-model="email"
              :label="$t('auth.email')"
              prepend-inner-icon="mdi-email"
              type="email"
              :rules="[rules.required, rules.email]"
              autofocus
              autocomplete="email"
              autocapitalize="none"
              spellcheck="false"
            />
            <!-- No auto-capitalisation, no autocorrect: iOS capitalises the first
                 letter of a text field by default, and the server matches the
                 username exactly — "goran" typed on an iPhone arrived as
                 "Goran" and was refused as invalid credentials. -->
            <v-text-field
              v-model="username"
              :label="$t('auth.username')"
              prepend-inner-icon="mdi-account"
              :rules="[rules.required, rules.minLength(3)]"
              autocomplete="username"
              autocapitalize="none"
              autocorrect="off"
              spellcheck="false"
            />
            <v-text-field
              v-model="displayName"
              :label="$t('auth.displayName')"
              prepend-inner-icon="mdi-badge-account"
              :rules="[rules.required]"
              autocomplete="name"
            />
            <v-text-field
              v-model="password"
              :label="$t('auth.password')"
              prepend-inner-icon="mdi-lock"
              :type="showPassword ? 'text' : 'password'"
              :append-inner-icon="showPassword ? 'mdi-eye-off' : 'mdi-eye'"
              :rules="[rules.required, rules.minLength(6)]"
              autocomplete="new-password"
              @click:append-inner="showPassword = !showPassword"
            />
            <!-- FR-88: optional, never required, never blocks sign-up. It
                 covers what a link cannot: a URL said out loud, a screenshot
                 in a chat, a conversation. -->
            <v-select
              v-if="ATTRIBUTION_ENABLED"
              v-model="heardAbout"
              :items="heardAboutItems"
              :label="$t('auth.heardAbout')"
              :hint="$t('auth.heardAboutHint')"
              persistent-hint
              prepend-inner-icon="mdi-bullhorn-outline"
              clearable
              class="mb-4"
              data-testid="heard-about"
            />

            <!-- FR-129 A2: a person sent here by the pairing page is about to
                 approve a device into an organization, so the account can be
                 born with one (the server creates it from `tenant_name` +
                 `tenant_slug`). Optional: the pairing page creates one too.
                 Shown by the pending-pairing marker, never by anything in the
                 URL, so the ordinary sign-up page is unchanged. -->
            <template v-if="forPairing">
              <v-divider class="my-4" />
              <div class="text-subtitle-2 mb-1" data-test="register-org">{{ $t('auth.pairOrgTitle') }}</div>
              <p class="text-body-2 text-medium-emphasis mb-3">{{ $t('auth.pairOrgHint') }}</p>
              <v-text-field
                v-model="orgName"
                :label="$t('auth.orgName')"
                prepend-inner-icon="mdi-domain"
                autocomplete="organization"
                @update:model-value="autoSlug"
              />
              <v-text-field
                v-model="orgSlug"
                :label="$t('auth.orgSlug')"
                :hint="$t('auth.orgSlugHint')"
                prepend-inner-icon="mdi-link-variant"
                :rules="[orgSlugRule, rules.slug]"
                autocapitalize="none"
                autocorrect="off"
                spellcheck="false"
                @update:model-value="slugTouched = true"
              />
            </template>

            <v-alert v-if="auth.error" type="error" density="compact" class="mb-4">
              {{ auth.error }}
            </v-alert>

            <!-- FR-129 A2: on production the account needs its email
                 activated before it can sign in; while a pairing waits, say
                 so here rather than leaving for the dashboard, and say that
                 signing in afterwards leads back to the pairing. -->
            <v-alert v-if="pairingAwaitsActivation" type="success" variant="tonal" class="mb-4" data-test="register-activation">
              <div>{{ pairingAwaitsActivation }}</div>
              <div class="mt-2">{{ $t('auth.pairAfterActivation') }}</div>
            </v-alert>

            <v-btn
              v-if="!pairingAwaitsActivation"
              type="submit"
              color="primary"
              block
              size="large"
              :loading="auth.loading"
            >
              {{ $t('auth.register') }}
            </v-btn>
          </v-form>

          <v-divider class="my-4" />
          <div class="text-center text-body-2 mb-2">{{ $t('auth.orRegisterWith') }}</div>
          <div class="d-flex flex-wrap justify-center ga-2 mb-4">
            <v-btn
              v-for="p in oauthProviders"
              :key="p.name"
              @click="oauthRegister(p.name)"
              :color="p.color"
              variant="outlined"
              :size="xs ? 'default' : 'small'"
            >
              <v-icon start>{{ p.icon }}</v-icon>
              {{ p.label }}
            </v-btn>
          </div>

          <v-card-text class="text-center">
            {{ $t('auth.hasAccount') }}
            <router-link :to="{ path: '/login', query: onward }">{{ $t('auth.login') }}</router-link>
          </v-card-text>
        </v-card>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useDisplay } from 'vuetify'
import { useI18n } from 'vue-i18n'
import { useAuthStore } from '@/stores/auth'
import { useWsStore } from '@/stores/ws'
import { useValidation } from '@/composables/useValidation'
import { useBrand } from '@/composables/useBrand'
import {
  ATTRIBUTION_ENABLED,
  SELF_REPORTED_OPTIONS,
  carriedQuery,
  signupAttribution,
  startOAuth,
  type Landing,
  type SelfReported,
  type SignupAttribution,
} from '@/utils/attribution'
import { trackGoal } from '@/utils/goals'
import { PAIR_PATH, hasPendingPair, takePendingPair } from '@/utils/pairing'

const auth = useAuthStore()
// Provider buttons are a real touch target on a phone (28 px at `small`).
const { xs } = useDisplay()
const ws = useWsStore()
const router = useRouter()
const route = useRoute()
const { rules } = useValidation()
const { t } = useI18n()
const { mark: brandMark } = useBrand()

const formRef = ref()
const email = ref('')
const username = ref('')
const displayName = ref('')
const password = ref('')
const showPassword = ref(false)

// FR-88 §3a — "How did you hear about Roomler?", sent as `self_reported`.
const heardAbout = ref<SelfReported | null>(null)
const heardAboutItems = computed(() =>
  SELF_REPORTED_OPTIONS.map((value) => ({ value, title: t(`auth.heardAboutOptions.${value}`) })),
)

/**
 * FR-88 §3a — carry, don't store: read from this page's URL at the moment of
 * sending, never kept anywhere. The static pages put the campaign keys on the
 * link that brought the visitor here; a visitor who landed on this page
 * directly gets `referrer_host`/`landing_path` from this page load.
 */
function landing(): Landing {
  return { referrer: document.referrer, host: window.location.host, path: route.path }
}
function attribution(): SignupAttribution | undefined {
  return signupAttribution(route.query, landing(), heardAbout.value)
}
/** The same keys on the link to the sign-in page, whose provider buttons
 *  create an account too. */
const onward = computed(() => carriedQuery(route.query, landing()))

const inviteCode = computed(() => (route.query.invite as string) || sessionStorage.getItem('pending_invite_code') || undefined)

// FR-129 A2 — a pairing waiting in this browser (`utils/pairing.ts`). Read
// once, when the page opens: the fields it reveals must not vanish under
// the person's hands when the marker runs out mid-form.
const forPairing = ref(hasPendingPair())
const orgName = ref('')
const orgSlug = ref('')
const slugTouched = ref(false)
/** The dashboard's rule: the slug follows the name until the person edits it. */
function autoSlug() {
  if (!slugTouched.value) {
    orgSlug.value = orgName.value
      .toLowerCase()
      .trim()
      .replace(/[^a-z0-9]+/g, '-')
      .replace(/^-+|-+$/g, '')
  }
}
/** A name without a slug would be silently dropped by the server (it needs both). */
function orgSlugRule(v: string) {
  return !orgName.value.trim() || !!v || t('auth.orgSlugNeeded')
}
function orgToCreate(): { name: string; slug: string } | undefined {
  const name = orgName.value.trim()
  const slug = orgSlug.value.trim()
  return forPairing.value && name && slug ? { name, slug } : undefined
}
/** The server's "check your email" answer, kept on screen while a pairing waits. */
const pairingAwaitsActivation = ref<string | null>(null)

const oauthProviders = [
  { name: 'google', label: 'Google', icon: 'mdi-google', color: '#DB4437' },
  { name: 'facebook', label: 'Facebook', icon: 'mdi-facebook', color: '#4267B2' },
  { name: 'github', label: 'GitHub', icon: 'mdi-github', color: '#333' },
  { name: 'linkedin', label: 'LinkedIn', icon: 'mdi-linkedin', color: '#0077B5' },
  { name: 'microsoft', label: 'Microsoft', icon: 'mdi-microsoft', color: '#00A4EF' },
]

/** The server parks the attribution under the CSRF state it mints and
 *  attaches it only if the callback creates the account. */
function oauthRegister(provider: string) {
  startOAuth(provider, attribution())
}

async function handleRegister() {
  const { valid } = await formRef.value.validate()
  if (!valid) return
  try {
    const result = await auth.register(
      email.value,
      username.value,
      password.value,
      displayName.value,
      inviteCode.value,
      attribution(),
      orgToCreate(),
    )
    // FR-88 §3b: on the server's yes, and before navigating, so the goal is
    // recorded on the page the visitor signed up from.
    trackGoal('signup')
    ws.connect()
    sessionStorage.removeItem('pending_invite_code')
    if (result?.invite_tenant) {
      router.push(`/tenant/${result.invite_tenant.tenant_id}`)
    } else if (!result?.user && forPairing.value) {
      // FR-129 A2 — no session yet: production wants the email activated
      // first. Stay here and say so; the marker in localStorage brings the
      // tab that signs in afterwards back to the pairing.
      pairingAwaitsActivation.value = result?.message || t('auth.pairCheckEmail')
    } else if (result?.user && takePendingPair()) {
      // A session arrived with the account (the auto-verified overlay):
      // straight back to the pairing.
      router.push(PAIR_PATH)
    } else {
      router.push({ name: 'dashboard' })
    }
  } catch {
    // error handled by store
  }
}
</script>
