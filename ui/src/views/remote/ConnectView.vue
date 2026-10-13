<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-container class="d-flex align-center justify-center" style="min-height: 80vh">
    <v-card max-width="460" width="100%" class="pa-2">
      <v-card-title>Connect to a device</v-card-title>
      <v-card-text>
        <p class="mb-4">
          Enter the connect code and the password the device's owner gave you. The device
          checks the password itself — this server never sees it — and whoever is at the
          device may still be asked to approve.
        </p>
        <v-form @submit.prevent="submit">
          <v-text-field
            v-model="code"
            label="Connect code"
            placeholder="XXXX-XXXX-XXXX"
            autocomplete="off"
            spellcheck="false"
            :disabled="busy"
            class="mb-2"
          />
          <v-text-field
            v-model="password"
            label="Device password"
            type="password"
            name="device-password"
            autocomplete="off"
            :disabled="busy"
          />
          <v-alert v-if="error" type="error" variant="tonal" class="mb-4">{{ error }}</v-alert>
          <div class="d-flex justify-end">
            <v-btn
              type="submit"
              variant="flat"
              color="primary"
              :loading="busy"
              :disabled="busy || !code.trim() || !password"
            >
              Connect
            </v-btn>
          </div>
        </v-form>
      </v-card-text>
    </v-card>
  </v-container>
</template>

<script setup lang="ts">
/**
 * FR-52 P4e — an outsider's way in to a device: its connect code and the
 * password its owner set on it.
 *
 * The login runs here (`externalLogin`); what it leaves — the attempt and the
 * key — is handed to the viewer in memory, once. The password lives in this
 * component's state only while the login runs, and is cleared after.
 *
 * ⚠️ No automatic retry, anywhere: every login the device answers costs it a
 * guess (spec §4b), so a retry is the person's decision, not the page's.
 */
import { ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useWsStore } from '@/stores/ws'
import {
  externalLogin,
  friendlyExtauthRefusal,
  handOffExternalSession,
} from '@/composables/useExternalLogin'

const route = useRoute()
const router = useRouter()
const auth = useAuthStore()
const ws = useWsStore()

const code = ref(typeof route.query.code === 'string' ? route.query.code : '')
const password = ref('')
const busy = ref(false)
const error = ref<string | null>(null)

/**
 * Who is logging in. The layout fetches the signed-in user only after it
 * mounts (`useAuth`), which on a fresh load of this page is after the route's
 * own guards: measured ~600 ms after `load`, longer on a slow link. A click in
 * that window waits for it, rather than telling a signed-in person to sign in
 * again. `null` once the session turns out not to be real, or never arrives.
 */
async function principalReady(timeoutMs = 10_000): Promise<string | null> {
  const deadline = Date.now() + timeoutMs
  while (!auth.user?.id) {
    if (!auth.isAuthenticated || Date.now() > deadline) return null
    await new Promise((r) => setTimeout(r, 100))
  }
  return auth.user.id
}

/** The login rides the user socket; give it a moment to open after a navigation. */
async function socketReady(timeoutMs = 10_000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs
  while (ws.status !== 'connected') {
    if (Date.now() > deadline) return false
    await new Promise((r) => setTimeout(r, 100))
  }
  return true
}

async function submit() {
  const connectCode = code.value.trim().toUpperCase()
  if (!connectCode || !password.value || busy.value) return
  busy.value = true
  error.value = null
  try {
    const principal = await principalReady()
    if (!principal) {
      error.value = 'Sign in again, then connect.'
      return
    }
    if (!(await socketReady())) {
      error.value = 'Not connected to the server. Check your connection and try again.'
      return
    }
    const result = await externalLogin(connectCode, password.value, principal)
    if (!result.ok) {
      error.value = friendlyExtauthRefusal(result.reason, result.retryAfterSecs)
      return
    }
    handOffExternalSession({
      connectCode,
      attemptId: result.attemptId,
      appKey: result.appKey,
    })
    await router.push({ name: 'connect-session', query: { code: connectCode } })
  } catch {
    error.value = 'The login could not be completed. Try again.'
  } finally {
    password.value = ''
    busy.value = false
  }
}
</script>
