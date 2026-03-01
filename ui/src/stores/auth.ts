import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import { api } from '@/api/client'
import router from '@/plugins/router'
import { subscribePush, unsubscribePush } from '@/composables/usePush'

interface User {
  id: string
  email: string
  username: string
  display_name: string
  avatar?: string
}

export const useAuthStore = defineStore('auth', () => {
  const user = ref<User | null>(null)
  const token = ref<string | null>(localStorage.getItem('access_token'))
  const loading = ref(false)
  const error = ref<string | null>(null)

  const isAuthenticated = computed(() => !!token.value)

  async function login(username: string, password: string) {
    loading.value = true
    error.value = null
    try {
      const data = await api.post<{ access_token: string; user: User }>('/auth/login', {
        username,
        password,
      })
      token.value = data.access_token
      user.value = data.user
      localStorage.setItem('access_token', data.access_token)
      subscribePush().catch(() => {})
    } catch (e) {
      error.value = (e as Error).message
      throw e
    } finally {
      loading.value = false
    }
  }

  async function register(
    email: string,
    username: string,
    password: string,
    displayName: string,
    inviteCode?: string,
  ) {
    loading.value = true
    error.value = null
    try {
      const body: Record<string, unknown> = {
        email,
        username,
        password,
        display_name: displayName,
      }
      if (inviteCode) body.invite_code = inviteCode

      const data = await api.post<{
        access_token: string
        user: User
        invite_tenant?: { tenant_id: string; tenant_name: string; tenant_slug: string }
      }>('/auth/register', body)
      token.value = data.access_token
      user.value = data.user
      localStorage.setItem('access_token', data.access_token)
      subscribePush().catch(() => {})
      return data
    } catch (e) {
      error.value = (e as Error).message
      throw e
    } finally {
      loading.value = false
    }
  }

  async function fetchMe() {
    if (!token.value) return
    try {
      user.value = await api.get<User>('/auth/me')
      subscribePush().catch(() => {})
    } catch {
      logout()
    }
  }

  function logout() {
    unsubscribePush().catch(() => {})
    token.value = null
    user.value = null
    localStorage.removeItem('access_token')
    router.push({ name: 'login' })
  }

  return { user, token, loading, error, isAuthenticated, login, register, fetchMe, logout }
})
