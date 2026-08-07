import { watch } from 'vue'
import { useRoute } from 'vue-router'
import { api } from '@/api/client'

/**
 * Route-change beacon for the platform analytics.
 *
 * Batched and fire-and-forget: navigation must never wait on (or fail
 * because of) analytics. Paths are sent as-is and NORMALISED server-side
 * — the server collapses ids to `:id`, so nothing here needs to know
 * which segments are sensitive.
 *
 * Installed once, from the authenticated app shell.
 */
const FLUSH_MS = 5_000
const MAX_BATCH = 50

export function usePageViews() {
  const route = useRoute()
  let queue: string[] = []
  let timer: ReturnType<typeof setTimeout> | null = null

  function flush() {
    timer = null
    if (!queue.length) return
    const paths = queue.slice(0, MAX_BATCH)
    queue = []
    const tenantId =
      typeof route.params.tenantId === 'string' ? route.params.tenantId : undefined
    // Swallow everything: a failed beacon is not a user-visible event,
    // and a 404 (stats disabled) must not surface as an error toast.
    void api.post('/stats/pageview', { paths, tenant_id: tenantId }).catch(() => {})
  }

  watch(
    () => route.fullPath,
    (path) => {
      queue.push(path)
      if (queue.length >= MAX_BATCH) {
        flush()
      } else if (!timer) {
        timer = setTimeout(flush, FLUSH_MS)
      }
    },
    { immediate: true },
  )

  // Best-effort flush when the tab goes away — otherwise the last few
  // views of a session are lost on close.
  if (typeof document !== 'undefined') {
    document.addEventListener('visibilitychange', () => {
      if (document.hidden) flush()
    })
  }
}
