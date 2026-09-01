// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import type { ServerTutorialState } from '@/composables/useTutorialProgress'
import { ref, computed } from 'vue'
import { api } from '@/api/client'
import router from '@/plugins/router'
import { subscribePush, unsubscribePush } from '@/composables/usePush'
import { markSignedIn, clearSignedIn, looksSignedIn } from '@/api/session'

interface User {
  id: string
  email: string
  username: string
  display_name: string
  avatar?: string
  /** Platform-operator allowlist member (stats PR-4) — gates the
   *  Observability nav/view; the server re-checks every request. */
  is_platform_admin?: boolean
  /** FR-12 P3 — the account's tutorial state, so progress follows the person
   *  and not the browser profile. Absent against an older server. */
  tutorial?: ServerTutorialState
}

export const useAuthStore = defineStore('auth', () => {
  const user = ref<User | null>(null)
  const loading = ref(false)
  const error = ref<string | null>(null)

  /**
   * Does this browser believe it is signed in?
   *
   * Seeded from the local hint so a hard refresh can render the app shell
   * without waiting for `/auth/me`, then corrected by the server: `fetchMe`
   * logs out if the session is not real. The credential itself is an HttpOnly
   * cookie this code cannot see — see `@/api/session`.
   */
  const signedIn = ref(looksSignedIn())
  const isAuthenticated = computed(() => signedIn.value)

  async function login(username: string, password: string) {
    loading.value = true
    error.value = null
    try {
      // The response still carries tokens; we deliberately ignore them. The
      // session arrived as Set-Cookie on this same response.
      const data = await api.post<{ user: User }>('/auth/login', {
        username,
        password,
      })
      user.value = data.user
      signedIn.value = true
      markSignedIn()
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
        access_token?: string
        user?: User
        invite_tenant?: { tenant_id: string; tenant_name: string; tenant_slug: string }
      }>('/auth/register', body)
      // Production registration returns no tokens and no user — the account
      // needs email activation first, so this is NOT a session yet. Only the
      // auto-verified path (e2e overlay) signs in, and it sets cookies.
      if (data.user) {
        user.value = data.user
        signedIn.value = true
        markSignedIn()
        subscribePush().catch(() => {})
      }
      return data
    } catch (e) {
      error.value = (e as Error).message
      throw e
    } finally {
      loading.value = false
    }
  }

  async function fetchMe() {
    if (!signedIn.value) return
    try {
      user.value = await api.get<User>('/auth/me')
      subscribePush().catch(() => {})
    } catch {
      // The hint said signed in and the server disagreed. It is the authority.
      await logout()
    }
  }

  /**
   * Sign out.
   *
   * ⚠️ The `POST /auth/logout` call is the load-bearing part and it was
   * MISSING: this used to clear `localStorage` and navigate, which looked like
   * a logout only because the SPA then had no token to send. The session
   * COOKIE survived — and the server has always accepted it — so the session
   * was still live for its full 7 days. Now that the cookie IS the credential,
   * a logout that does not ask the server to expire it does not log anyone out.
   *
   * Best-effort: if the request fails (offline, server down) we still clear
   * locally rather than trapping the user in a session they asked to leave.
   */
  async function logout() {
    unsubscribePush().catch(() => {})
    try {
      await api.post('/auth/logout', {})
    } catch {
      /* clear locally regardless — see above */
    }
    user.value = null
    signedIn.value = false
    clearSignedIn()
    router.push({ name: 'login' })
  }

  return { user, loading, error, isAuthenticated, login, register, fetchMe, logout }
})
