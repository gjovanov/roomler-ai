<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!--
  FR-129 (#3022) A2 — the pairing page an installer sends a person to.

  The installer shows an eight-letter code and says "open roomler.ai/pair".
  The person types the code here (the URL never carries it, decision 1),
  sees the device that asked — its name, system, role, installer, country
  and when it started — picks the organization it joins, and approves or
  denies. The installer polls the server and continues on its own.

  Three states by the session: signed out (sign in, sign up or a provider,
  all of which come back here), signed in with no code yet, and the review.
  Reached from a phone while the installer runs on the PC, so every width
  is a first-class one (docs/ui.md).
-->
<template>
  <v-container class="fill-height pa-2 pa-md-4 pa-xl-6 page-no-sideways" fluid>
    <v-row align="center" justify="center">
      <v-col cols="12" sm="10" md="7" lg="5">
        <v-card class="pa-4 pa-md-6">
          <v-card-title class="text-h5 text-md-h4 text-wrap px-0 mb-2 d-flex align-center">
            <img :src="brandMark" alt="" width="28" height="28" class="mr-2 flex-0-0" />
            <span>{{ $t('pair.title') }}</span>
          </v-card-title>

          <!-- Signed out: say what this is, and offer every way in. Each one
               leaves the pending-pairing marker (localStorage) and the
               same-tab return (sessionStorage) so the person comes back. -->
          <template v-if="!auth.isAuthenticated">
            <p class="text-body-1 mb-4" data-test="pair-signed-out">{{ $t('pair.signedOutIntro') }}</p>
            <div class="d-flex flex-column ga-3 mb-4">
              <v-btn color="primary" block size="large" data-test="pair-sign-in" @click="leaveFor('/login')">
                {{ $t('pair.signIn') }}
              </v-btn>
              <v-btn variant="outlined" block size="large" data-test="pair-register" @click="leaveFor('/register')">
                {{ $t('pair.createAccount') }}
              </v-btn>
            </div>
            <div class="text-center text-body-2 mb-2">{{ $t('pair.orContinueWith') }}</div>
            <div class="d-flex flex-wrap justify-center ga-2">
              <v-btn
                v-for="p in oauthProviders"
                :key="p.name"
                :color="p.color"
                variant="outlined"
                :size="xs ? 'default' : 'small'"
                @click="oauth(p.name)"
              >
                <v-icon start>{{ p.icon }}</v-icon>
                {{ p.label }}
              </v-btn>
            </div>
          </template>

          <!-- Signed in: the code. -->
          <template v-else-if="state === 'enter'">
            <p class="text-body-1 mb-4">{{ $t('pair.intro') }}</p>
            <v-form @submit.prevent="lookup">
              <v-text-field
                :model-value="codeShown"
                :label="$t('pair.codeLabel')"
                :hint="$t('pair.codeHint')"
                persistent-hint
                prepend-inner-icon="mdi-key-variant"
                class="pair-code mb-4"
                autocomplete="off"
                autocapitalize="characters"
                autocorrect="off"
                spellcheck="false"
                autofocus
                data-test="pair-code"
                @update:model-value="onCodeInput"
              />
              <v-alert
                v-if="lookupError"
                type="error"
                variant="tonal"
                density="compact"
                class="mb-4"
                data-test="pair-lookup-error"
              >
                {{ lookupError }}
              </v-alert>
              <v-btn
                type="submit"
                color="primary"
                block
                size="large"
                :disabled="!complete || busy"
                :loading="busy"
                data-test="pair-lookup"
              >
                {{ $t('pair.lookup') }}
              </v-btn>
            </v-form>
          </template>

          <!-- The device, the warning, the organization, the decision. -->
          <template v-else-if="state === 'review' && device">
            <div class="text-subtitle-1 font-weight-medium mb-1">{{ $t('pair.deviceTitle') }}</div>
            <v-list density="compact" class="pair-facts mb-2" data-test="pair-device">
              <v-list-item prepend-icon="mdi-laptop" :title="device.machine_name" :subtitle="$t('pair.machine')" />
              <v-list-item prepend-icon="mdi-chip" :title="systemLabel" :subtitle="$t('pair.system')" />
              <v-list-item prepend-icon="mdi-monitor-share" :title="roleLabel" :subtitle="$t('pair.role')" />
              <v-list-item prepend-icon="mdi-package-variant" :title="clientLabel" :subtitle="$t('pair.client')" />
              <v-list-item prepend-icon="mdi-map-marker" :title="locationLabel" :subtitle="$t('pair.location')" />
              <v-list-item prepend-icon="mdi-clock-outline" :title="startedLabel" :subtitle="$t('pair.started')" />
            </v-list>
            <p v-if="device.halves === 2" class="text-body-2 text-medium-emphasis mb-3" data-test="pair-halves">
              {{ $t('pair.halvesTwo') }}
            </p>

            <v-alert type="warning" variant="tonal" icon="mdi-shield-alert" class="mb-4" data-test="pair-warning">
              <div class="font-weight-bold">{{ $t('pair.warning') }}</div>
              <div class="text-body-2 mt-1">{{ $t('pair.warningDetail') }}</div>
            </v-alert>

            <div v-if="!orgsLoaded" class="text-center py-2">
              <v-progress-circular indeterminate color="primary" size="24" />
            </div>
            <v-select
              v-else-if="tenantStore.tenants.length > 0"
              v-model="orgId"
              :items="tenantStore.tenants"
              item-title="name"
              item-value="id"
              :label="$t('pair.orgLabel')"
              :hint="$t('pair.orgHint')"
              persistent-hint
              prepend-inner-icon="mdi-domain"
              class="mb-4"
              data-test="pair-org"
            />
            <template v-else>
              <p class="text-body-2 mb-2" data-test="pair-no-orgs">{{ $t('pair.noOrgs') }}</p>
              <v-form ref="orgFormRef" data-test="pair-create-org" @submit.prevent="createOrg">
                <v-text-field
                  v-model="orgName"
                  :label="$t('pair.orgName')"
                  :rules="[rules.required]"
                  autocomplete="organization"
                  @update:model-value="autoSlug"
                />
                <v-text-field
                  v-model="orgSlug"
                  :label="$t('pair.orgSlug')"
                  :hint="$t('pair.orgSlugHint')"
                  :rules="[rules.required, rules.slug]"
                  autocapitalize="none"
                  autocorrect="off"
                  spellcheck="false"
                  @update:model-value="slugTouched = true"
                />
                <v-alert v-if="orgError" type="error" density="compact" class="mb-3" data-test="pair-org-error">
                  {{ orgError }}
                </v-alert>
                <v-btn type="submit" variant="outlined" block :loading="creatingOrg" class="mb-4" data-test="pair-org-create">
                  {{ $t('pair.createOrg') }}
                </v-btn>
              </v-form>
            </template>

            <v-alert
              v-if="decideError"
              type="error"
              variant="tonal"
              density="compact"
              class="mb-4"
              data-test="pair-decide-error"
            >
              <div>{{ decideError }}</div>
              <div v-if="decideDetail" class="text-body-2 mt-1">{{ decideDetail }}</div>
            </v-alert>

            <!-- Approve first on a desktop (the right), first on a phone (the top). -->
            <div class="d-flex flex-column flex-sm-row-reverse ga-2">
              <v-btn
                color="primary"
                size="large"
                class="flex-grow-1"
                :disabled="!orgId || busy"
                :loading="busy && pending === 'approve'"
                data-test="pair-approve"
                @click="decide('approve')"
              >
                {{ $t('pair.approve') }}
              </v-btn>
              <v-btn
                variant="outlined"
                color="error"
                size="large"
                class="flex-grow-1"
                :disabled="busy"
                :loading="busy && pending === 'deny'"
                data-test="pair-deny"
                @click="decide('deny')"
              >
                {{ $t('pair.deny') }}
              </v-btn>
            </div>
          </template>

          <v-alert v-else-if="state === 'approved'" type="success" variant="tonal" data-test="pair-approved">
            {{ $t('pair.approved') }}
          </v-alert>

          <v-alert v-else-if="state === 'denied'" type="info" variant="tonal" data-test="pair-denied">
            {{ $t('pair.denied') }}
          </v-alert>

          <template v-else-if="state === 'gone'">
            <v-alert type="error" variant="tonal" class="mb-4" data-test="pair-gone">{{ $t('pair.gone') }}</v-alert>
            <v-btn variant="outlined" block size="large" data-test="pair-start-again" @click="reset">
              {{ $t('pair.startAgain') }}
            </v-btn>
          </template>
        </v-card>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useDisplay } from 'vuetify'
import { useI18n } from 'vue-i18n'
import { api, ApiError } from '@/api/client'
import { useAuthStore } from '@/stores/auth'
import { useTenantStore } from '@/stores/tenant'
import { useBrand } from '@/composables/useBrand'
import { useValidation } from '@/composables/useValidation'
import { signupAttribution, startOAuth, type Landing } from '@/utils/attribution'
import {
  PAIR_PATH,
  archLabel,
  clearPendingPair,
  countryName,
  elapsedSince,
  formatElapsed,
  formatUserCode,
  isCompleteUserCode,
  normalizeUserCode,
  rememberPendingPair,
} from '@/utils/pairing'

/** `POST /api/pair/lookup`'s answer (A1's contract). */
interface PendingPairing {
  machine_name: string
  os: string
  arch: string
  role: string
  halves: number
  client: string
  client_version: string
  country: string | null
  started_at: string
  expires_at: string
}

type State = 'enter' | 'review' | 'approved' | 'denied' | 'gone'

const auth = useAuthStore()
const tenantStore = useTenantStore()
const router = useRouter()
const route = useRoute()
const { xs } = useDisplay()
const { t, te, locale } = useI18n()
const { rules } = useValidation()
const { mark: brandMark } = useBrand()

// ── Signed out ──────────────────────────────────────────────────────────────

const oauthProviders = [
  { name: 'google', label: 'Google', icon: 'mdi-google', color: '#DB4437' },
  { name: 'facebook', label: 'Facebook', icon: 'mdi-facebook', color: '#4267B2' },
  { name: 'github', label: 'GitHub', icon: 'mdi-github', color: '#333' },
  { name: 'linkedin', label: 'LinkedIn', icon: 'mdi-linkedin', color: '#0077B5' },
  { name: 'microsoft', label: 'Microsoft', icon: 'mdi-microsoft', color: '#00A4EF' },
]

/**
 * Both returns, before leaving: the same-tab one the sign-in page, the OAuth
 * callback and the router guard already honour (`pending_redirect`, a
 * same-origin literal), and the cross-tab marker for an email sign-up whose
 * activation link opens a new tab (`utils/pairing.ts`).
 */
function keepReturn() {
  rememberPendingPair()
  try {
    sessionStorage.setItem('pending_redirect', PAIR_PATH)
  } catch {
    /* private mode: the localStorage marker, or neither — the page still works */
  }
}

function leaveFor(path: '/login' | '/register') {
  keepReturn()
  router.push(path)
}

function landing(): Landing {
  return { referrer: document.referrer, host: window.location.host, path: route.path }
}

/** A provider sign-in creates the account when there is none (FR-88). */
function oauth(provider: string) {
  keepReturn()
  startOAuth(provider, signupAttribution(route.query, landing()))
}

// ── The code ────────────────────────────────────────────────────────────────

const state = ref<State>('enter')
/** The eight letters, normalized; what the server receives. */
const code = ref('')
const codeShown = computed(() => formatUserCode(code.value))
const complete = computed(() => isCompleteUserCode(code.value))
const busy = ref(false)
const lookupError = ref<string | null>(null)

function onCodeInput(raw: string | null) {
  code.value = normalizeUserCode(raw ?? '')
  lookupError.value = null
}

async function lookup() {
  if (!complete.value || busy.value) return
  busy.value = true
  lookupError.value = null
  try {
    device.value = await api.post<PendingPairing>('/pair/lookup', { user_code: code.value })
    orgId.value = tenantStore.current?.id ?? tenantStore.tenants[0]?.id ?? null
    state.value = 'review'
  } catch (e) {
    const status = (e as { status?: number }).status
    // A 404 is also what a server without pairing answers, so the words
    // cover both without claiming either. A 429 carries the client's
    // rate-limit text (`Retry-After` included) as its message.
    lookupError.value = status === 404 ? t('pair.notFound') : (e as Error).message || t('pair.lookupFailed')
  } finally {
    busy.value = false
  }
}

// ── The device ──────────────────────────────────────────────────────────────

const device = ref<PendingPairing | null>(null)

/** Ticks so "started 2 minutes ago" stays true while the page is open. */
const now = ref(Date.now())
const clock = setInterval(() => {
  now.value = Date.now()
}, 30_000)
onUnmounted(() => clearInterval(clock))

const systemLabel = computed(() => {
  const d = device.value
  if (!d) return ''
  const osKey = `pair.os.${d.os}`
  const os = te(osKey) ? t(osKey) : d.os
  const arch = archLabel(d.arch)
  return arch ? `${os} · ${arch}` : os
})
const roleLabel = computed(() =>
  device.value?.role === 'tunnel' ? t('pair.roleTunnel') : t('pair.roleDaemon'),
)
const clientLabel = computed(() => `${device.value?.client ?? ''} ${device.value?.client_version ?? ''}`.trim())
const locationLabel = computed(() => countryName(device.value?.country, locale.value) ?? t('pair.unknownLocation'))
const startedLabel = computed(() => {
  const e = elapsedSince(device.value?.started_at, now.value)
  return e ? formatElapsed(e, locale.value, t('pair.justNow')) : '—'
})

// ── The organization ────────────────────────────────────────────────────────

const orgsLoaded = ref(false)
const orgId = ref<string | null>(null)
const orgFormRef = ref()
const orgName = ref('')
const orgSlug = ref('')
const slugTouched = ref(false)
const creatingOrg = ref(false)
const orgError = ref<string | null>(null)

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

async function createOrg() {
  const { valid } = await orgFormRef.value.validate()
  if (!valid) return
  creatingOrg.value = true
  orgError.value = null
  try {
    const tenant = await tenantStore.createTenant(orgName.value.trim(), orgSlug.value.trim())
    orgId.value = tenant.id
  } catch (e) {
    const msg = e instanceof Error ? e.message : ''
    // tenants.slug is globally unique, and a taken slug is the only 409 here.
    orgError.value =
      e instanceof ApiError && e.status === 409 ? t('pair.orgSlugTaken') : msg || t('pair.orgCreateFailed')
  } finally {
    creatingOrg.value = false
  }
}

// ── The decision ────────────────────────────────────────────────────────────

const pending = ref<'approve' | 'deny' | null>(null)
const decideError = ref<string | null>(null)
const decideDetail = ref<string | null>(null)

async function decide(action: 'approve' | 'deny') {
  if (busy.value) return
  if (action === 'approve' && !orgId.value) return
  busy.value = true
  pending.value = action
  decideError.value = null
  decideDetail.value = null
  try {
    if (action === 'approve') {
      await api.post('/pair/approve', { user_code: code.value, tenant_id: orgId.value })
      state.value = 'approved'
    } else {
      await api.post('/pair/deny', { user_code: code.value })
      state.value = 'denied'
    }
  } catch (e) {
    const status = (e as { status?: number }).status
    if (status === 404) {
      state.value = 'gone'
    } else if (status === 403) {
      // An answer, not an expired credential (CLAUDE.md): in the product's
      // words, with the server's underneath when it said more.
      decideError.value = device.value?.role === 'tunnel' ? t('pair.forbiddenTunnel') : t('pair.forbiddenDaemon')
      const said = ((e as { data?: { message?: string } }).data?.message ?? '').trim()
      decideDetail.value = said || null
    } else {
      decideError.value = (e as Error).message || t('pair.decideFailed')
    }
  } finally {
    busy.value = false
    pending.value = null
  }
}

function reset() {
  device.value = null
  code.value = ''
  lookupError.value = null
  decideError.value = null
  decideDetail.value = null
  state.value = 'enter'
}

onMounted(async () => {
  if (!auth.isAuthenticated) return
  // The person is here: whatever marker brought them is spent, and must not
  // send a later sign-in back here once this pairing is over.
  clearPendingPair()
  try {
    await tenantStore.fetchTenants()
  } catch {
    /* the picker shows what the store has; the server decides anyway */
  } finally {
    orgsLoaded.value = true
  }
})
</script>

<style scoped>
.pair-code :deep(input) {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 1.35rem;
  letter-spacing: 0.12em;
  text-transform: uppercase;
}
.pair-facts :deep(.v-list-item-title) {
  white-space: normal;
  overflow-wrap: anywhere;
}
</style>
