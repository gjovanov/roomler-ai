<template>
  <v-container class="fill-height" fluid>
    <v-row align="center" justify="center">
      <v-col cols="12" sm="8" md="4">
        <v-card class="pa-4">
          <v-card-title class="text-center text-h5 mb-4">
            <v-icon color="primary" class="mr-2">mdi-forum</v-icon>
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
            />
            <v-text-field
              v-model="username"
              :label="$t('auth.username')"
              prepend-inner-icon="mdi-account"
              :rules="[rules.required, rules.minLength(3)]"
            />
            <v-text-field
              v-model="displayName"
              :label="$t('auth.displayName')"
              prepend-inner-icon="mdi-badge-account"
              :rules="[rules.required]"
            />
            <v-text-field
              v-model="password"
              :label="$t('auth.password')"
              prepend-inner-icon="mdi-lock"
              type="password"
              :rules="[rules.required, rules.minLength(6)]"
            />

            <v-alert v-if="auth.error" type="error" density="compact" class="mb-4">
              {{ auth.error }}
            </v-alert>

            <v-btn
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
          <div class="text-center text-body-2 mb-2">Or register with</div>
          <div class="d-flex flex-wrap justify-center ga-2 mb-4">
            <v-btn
              v-for="p in oauthProviders"
              :key="p.name"
              @click="oauthRegister(p.name)"
              :color="p.color"
              variant="outlined"
              size="small"
            >
              <v-icon start>{{ p.icon }}</v-icon>
              {{ p.label }}
            </v-btn>
          </div>

          <v-card-text class="text-center">
            {{ $t('auth.hasAccount') }}
            <router-link to="/login">{{ $t('auth.login') }}</router-link>
          </v-card-text>
        </v-card>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useWsStore } from '@/stores/ws'
import { useValidation } from '@/composables/useValidation'

const auth = useAuthStore()
const ws = useWsStore()
const router = useRouter()
const route = useRoute()
const { rules } = useValidation()

const formRef = ref()
const email = ref('')
const username = ref('')
const displayName = ref('')
const password = ref('')

const inviteCode = computed(() => (route.query.invite as string) || sessionStorage.getItem('pending_invite_code') || undefined)

const oauthProviders = [
  { name: 'google', label: 'Google', icon: 'mdi-google', color: '#DB4437' },
  { name: 'facebook', label: 'Facebook', icon: 'mdi-facebook', color: '#4267B2' },
  { name: 'github', label: 'GitHub', icon: 'mdi-github', color: '#333' },
  { name: 'linkedin', label: 'LinkedIn', icon: 'mdi-linkedin', color: '#0077B5' },
  { name: 'microsoft', label: 'Microsoft', icon: 'mdi-microsoft', color: '#00A4EF' },
]

function oauthRegister(provider: string) {
  window.location.href = `/api/oauth/${provider}`
}

async function handleRegister() {
  const { valid } = await formRef.value.validate()
  if (!valid) return
  try {
    const result = await auth.register(email.value, username.value, password.value, displayName.value, inviteCode.value)
    ws.connect(auth.token!)
    sessionStorage.removeItem('pending_invite_code')
    if (result?.invite_tenant) {
      router.push(`/tenant/${result.invite_tenant.tenant_id}`)
    } else {
      router.push({ name: 'dashboard' })
    }
  } catch {
    // error handled by store
  }
}
</script>
