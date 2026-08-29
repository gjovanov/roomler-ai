// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { onMounted } from 'vue'
import { useAuthStore } from '@/stores/auth'
import { useWsStore } from '@/stores/ws'

export function useAuth() {
  const auth = useAuthStore()
  const ws = useWsStore()

  onMounted(async () => {
    if (auth.isAuthenticated) {
      await auth.fetchMe()
      if (auth.user) {
        ws.connect()
      }
    }
  })

  function logout() {
    ws.disconnect()
    auth.logout()
  }

  return { auth, logout }
}
