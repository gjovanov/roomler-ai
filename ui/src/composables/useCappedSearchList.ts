import { computed, ref, watch, type ComputedRef, type Ref } from 'vue'

/**
 * The sidebar "capped list + server search" behavior, shared by the Rooms and
 * Devices groups:
 *
 * - EMPTY query → a client-side slice of the complete `all` list (first 20,
 *   "Load more" reveals 20 more). The underlying store list stays COMPLETE —
 *   dashboard tiles, call badges and presence patching all iterate it, so the
 *   cap must be presentational only.
 * - NON-EMPTY query → debounced SERVER search into a separate result set
 *   (never clobbering `all`); "Load more" pages the server. `hasMore` is
 *   inferred from a full page (the endpoints return bare arrays).
 *
 * `reset()` returns to the empty-query first page — call it on tenant switch
 * and WS reconnect, the two places the underlying data is refetched wholesale.
 */
export interface CappedSearchOptions<T extends { id: string }> {
  all: Ref<T[]> | ComputedRef<T[]>
  /** Server search: (trimmed query, 1-based page) → one page of results. */
  search: (q: string, page: number) => Promise<T[]>
  pageSize?: number
  debounceMs?: number
}

export function useCappedSearchList<T extends { id: string }>(opts: CappedSearchOptions<T>) {
  const pageSize = opts.pageSize ?? 20
  const debounceMs = opts.debounceMs ?? 300

  const query = ref('')
  const visible = ref(pageSize)
  const results = ref<T[]>([]) as Ref<T[]>
  const searching = ref(false)
  const serverHasMore = ref(false)
  let serverPage = 1
  let timer: ReturnType<typeof setTimeout> | undefined
  let seq = 0

  const active = computed(() => query.value.trim().length > 0)

  const items = computed<T[]>(() =>
    active.value ? results.value : opts.all.value.slice(0, visible.value),
  )
  const hasMore = computed(() =>
    active.value ? serverHasMore.value : opts.all.value.length > visible.value,
  )

  async function runSearch(page: number) {
    const q = query.value.trim()
    if (!q) return
    const mySeq = ++seq
    searching.value = true
    try {
      const batch = await opts.search(q, page)
      if (mySeq !== seq) return // superseded — a newer query/page is in flight
      if (page === 1) {
        results.value = batch
      } else {
        // Dedup on append: the server pages by position, and rows can shift
        // between requests.
        const seen = new Set(results.value.map((r) => r.id))
        results.value = [...results.value, ...batch.filter((r) => !seen.has(r.id))]
      }
      serverPage = page
      serverHasMore.value = batch.length === pageSize
    } catch {
      if (mySeq === seq && page === 1) {
        results.value = []
        serverHasMore.value = false
      }
    } finally {
      if (mySeq === seq) searching.value = false
    }
  }

  watch(query, () => {
    if (timer) clearTimeout(timer)
    if (!query.value.trim()) {
      // Leaving search mode: invalidate any in-flight request so a slow
      // response can't repopulate the cleared results.
      seq++
      results.value = []
      serverHasMore.value = false
      searching.value = false
      return
    }
    timer = setTimeout(() => runSearch(1), debounceMs)
  })

  function loadMore() {
    if (active.value) void runSearch(serverPage + 1)
    else visible.value += pageSize
  }

  function reset() {
    if (timer) clearTimeout(timer)
    seq++
    query.value = ''
    visible.value = pageSize
    results.value = []
    serverHasMore.value = false
    searching.value = false
  }

  return { query, items, hasMore, searching, active, loadMore, reset }
}
