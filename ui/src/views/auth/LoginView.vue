<template>
  <v-container class="fill-height" fluid>
    <v-row align="center" justify="center">
      <v-col cols="12" sm="8" md="4">
        <v-card class="pa-4">
          <v-card-title class="text-center text-h5 mb-4">
            <v-icon color="primary" class="mr-2">mdi-forum</v-icon>
            {{ $t('auth.login') }}
          </v-card-title>

          <v-form @submit.prevent="handleLogin">
            <v-text-field
              v-model="username"
              :label="$t('auth.username')"
              prepend-inner-icon="mdi-account"
              required
              autofocus
            />
            <v-text-field
              v-model="password"
              :label="$t('auth.password')"
              prepend-inner-icon="mdi-lock"
              type="password"
              required
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
              {{ $t('auth.login') }}
            </v-btn>
          </v-form>

          <v-divider class="my-4" />
          <v-card-text class="text-center pb-2">
            {{ $t('auth.orLoginWith') || 'Or login with' }}
          </v-card-text>
          <div class="d-flex flex-wrap justify-center ga-2 px-4 pb-4">
            <v-btn
              v-for="p in oauthProviders"
              :key="p.name"
              @click="oauthLogin(p.name)"
              :color="p.color"
              variant="outlined"
              size="small"
            >
              <v-icon start>{{ p.icon }}</v-icon>
              {{ p.label }}
            </v-btn>
          </div>

          <v-card-text class="text-center">
            {{ $t('auth.noAccount') }}
            <router-link to="/register">{{ $t('auth.register') }}</router-link>
          </v-card-text>
        </v-card>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useWsStore } from '@/stores/ws'

const auth = useAuthStore()
const ws = useWsStore()
const router = useRouter()

const username = ref('')
const password = ref('')

const oauthProviders = [
  { name: 'google', label: 'Google', icon: 'mdi-google', color: '#DB4437' },
  { name: 'facebook', label: 'Facebook', icon: 'mdi-facebook', color: '#4267B2' },
  { name: 'github', label: 'GitHub', icon: 'mdi-github', color: '#333' },
  { name: 'linkedin', label: 'LinkedIn', icon: 'mdi-linkedin', color: '#0077B5' },
  { name: 'microsoft', label: 'Microsoft', icon: 'mdi-microsoft', color: '#00A4EF' },
]

function oauthLogin(provider: string) {
  window.location.href = `/api/oauth/${provider}`
}

async function handleLogin() {
  try {
    await auth.login(username.value, password.value)
    ws.connect(auth.token!)
    router.push({ name: 'dashboard' })
  } catch {
    // error handled by store
  }
}
</script>
