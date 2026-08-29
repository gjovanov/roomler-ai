// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { watch } from 'vue'
import { useAuthStore } from '@/stores/auth'
import { useWsStore } from '@/stores/ws'

export function useWebSocket() {
  const auth = useAuthStore()
  const ws = useWsStore()

  watch(
    () => auth.isAuthenticated,
    (signedIn) => {
      if (signedIn) {
        ws.connect()
      } else {
        ws.disconnect()
      }
    },
    { immediate: true },
  )

  return ws
}
