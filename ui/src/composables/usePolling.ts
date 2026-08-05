import { onMounted, onUnmounted } from 'vue'

/**
 * Stats polling helper: run `fn` immediately and every `ms`, skipping
 * ticks while the tab is hidden (a background dashboard must not burn
 * requests), stopping on unmount, and refetching once on the ws store's
 * `ws:reconnected` window event (the established resync hook).
 */
export function usePolling(fn: () => void | Promise<void>, ms: number) {
  let timer: ReturnType<typeof setInterval> | null = null

  const tick = () => {
    if (typeof document !== 'undefined' && document.hidden) return
    void fn()
  }
  const start = () => {
    stop()
    void fn()
    timer = setInterval(tick, ms)
  }
  const stop = () => {
    if (timer) {
      clearInterval(timer)
      timer = null
    }
  }
  const onReconnect = () => void fn()

  onMounted(() => {
    start()
    window.addEventListener('ws:reconnected', onReconnect)
  })
  onUnmounted(() => {
    stop()
    window.removeEventListener('ws:reconnected', onReconnect)
  })

  return { start, stop }
}
