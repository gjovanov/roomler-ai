<template>
  <v-container class="fill-height" fluid>
    <v-row align="center" justify="center">
      <v-col cols="12" sm="6" class="text-center">
        <v-progress-circular v-if="!error" indeterminate color="primary" size="64" />
        <v-alert v-else type="error" class="mt-4">
          {{ error }}
          <template #append>
            <v-btn variant="text" to="/login">Back to login</v-btn>
          </template>
        </v-alert>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { useRouter, useRoute } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useWsStore } from '@/stores/ws'

const router = useRouter()
const route = useRoute()
const auth = useAuthStore()
const ws = useWsStore()
const error = ref('')

onMounted(async () => {
  const token = route.query.token as string
  if (!token) {
    error.value = 'No token received from OAuth provider'
    return
  }

  try {
    // Store the token and fetch user info
    localStorage.setItem('access_token', token)
    auth.token = token
    await auth.fetchMe()
    ws.connect(token)
    router.push({ name: 'dashboard' })
  } catch (e) {
    error.value = 'Failed to complete OAuth login'
    localStorage.removeItem('access_token')
  }
})
</script>
