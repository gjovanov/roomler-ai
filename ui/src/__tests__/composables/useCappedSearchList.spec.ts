import { describe, expect, it, vi } from 'vitest'
import { nextTick, ref } from 'vue'
import { useCappedSearchList } from '@/composables/useCappedSearchList'

type Item = { id: string; name: string }

function items(n: number, prefix = 'i'): Item[] {
  return Array.from({ length: n }, (_, i) => ({ id: `${prefix}${i}`, name: `${prefix} ${i}` }))
}

describe('useCappedSearchList', () => {
  it('empty query: slices the complete list at pageSize; loadMore reveals more', () => {
    const all = ref(items(45))
    const list = useCappedSearchList<Item>({ all, search: vi.fn(), pageSize: 20 })
    expect(list.items.value).toHaveLength(20)
    expect(list.hasMore.value).toBe(true)
    list.loadMore()
    expect(list.items.value).toHaveLength(40)
    list.loadMore()
    expect(list.items.value).toHaveLength(45)
    expect(list.hasMore.value).toBe(false)
  })

  it('the underlying list is never mutated — the cap is presentational', () => {
    const all = ref(items(30))
    useCappedSearchList<Item>({ all, search: vi.fn(), pageSize: 20 })
    expect(all.value).toHaveLength(30)
  })

  it('non-empty query: debounced server search into a separate result set', async () => {
    vi.useFakeTimers()
    const all = ref(items(3, 'local'))
    const search = vi.fn().mockResolvedValue(items(20, 'srv'))
    const list = useCappedSearchList<Item>({ all, search, pageSize: 20, debounceMs: 300 })
    list.query.value = 'srv'
    expect(search).not.toHaveBeenCalled()
    await vi.advanceTimersByTimeAsync(300)
    expect(search).toHaveBeenCalledWith('srv', 1)
    expect(list.items.value[0]!.id).toBe('srv0')
    // Full page ⇒ more may exist server-side.
    expect(list.hasMore.value).toBe(true)
    // Load more pages the server and dedups on append.
    search.mockResolvedValue([...items(2, 'srv'), { id: 'srvX', name: 'new' }])
    list.loadMore()
    await vi.advanceTimersByTimeAsync(0)
    expect(search).toHaveBeenLastCalledWith('srv', 2)
    expect(list.items.value.filter((i) => i.id === 'srv0')).toHaveLength(1)
    expect(list.items.value.some((i) => i.id === 'srvX')).toBe(true)
    // A short page ends paging.
    expect(list.hasMore.value).toBe(false)
    vi.useRealTimers()
  })

  it('clearing the query leaves search mode and drops stale in-flight results', async () => {
    vi.useFakeTimers()
    const all = ref(items(5, 'local'))
    let resolveSearch!: (v: Item[]) => void
    const search = vi.fn(() => new Promise<Item[]>((r) => (resolveSearch = r)))
    const list = useCappedSearchList<Item>({ all, search, pageSize: 20, debounceMs: 100 })
    list.query.value = 'x'
    await vi.advanceTimersByTimeAsync(100)
    expect(search).toHaveBeenCalled()
    list.query.value = '' // back to the local slice
    await nextTick()
    expect(list.items.value[0]!.id).toBe('local0')
    resolveSearch(items(9, 'late')) // the slow response must be discarded
    await vi.advanceTimersByTimeAsync(0)
    expect(list.items.value[0]!.id).toBe('local0')
    expect(list.searching.value).toBe(false)
    vi.useRealTimers()
  })

  it('reset returns to the un-searched first page', async () => {
    vi.useFakeTimers()
    const all = ref(items(50))
    const search = vi.fn().mockResolvedValue(items(20, 'srv'))
    const list = useCappedSearchList<Item>({ all, search, pageSize: 20 })
    list.loadMore()
    list.query.value = 'srv'
    await vi.advanceTimersByTimeAsync(300)
    list.reset()
    expect(list.query.value).toBe('')
    expect(list.items.value).toHaveLength(20)
    expect(list.active.value).toBe(false)
    vi.useRealTimers()
  })
})
