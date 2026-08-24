import { watch } from 'vue'
import { useAuthStore } from '@/stores/auth'
import { useWsStore } from '@/stores/ws'

export function useWebSocket() {
  const auth = useAuthStore()
  const ws = useWsStore()

  watch(
    () => auth.token,
    (token) => {
      if (token) {
        ws.connect()
      } else {
        ws.disconnect()
      }
    },
    { immediate: true },
  )

  return ws
}
