import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import { useMembersStore, type Member } from '@/stores/members'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

const TENANT_ID = '69a1dbbad2000f26adc875ff'

function mkMember(over: Partial<Member> = {}): Member {
  return {
    id: 'm1',
    user_id: 'u1',
    nickname: null,
    display_name: 'Ada',
    role_ids: [],
    joined_at: '2026-01-01T00:00:00Z',
    ...over,
  }
}

function mkPage(items: Member[], over: Record<string, unknown> = {}) {
  return { items, total: items.length, page: 1, per_page: 25, total_pages: 1, ...over }
}

beforeEach(() => {
  setActivePinia(createPinia())
  vi.clearAllMocks()
})

describe('members store', () => {
  it('fetchMembers loads a page with pagination params', async () => {
    const store = useMembersStore()
    mockApi.get.mockResolvedValueOnce(mkPage([mkMember()], { total: 60, total_pages: 3, page: 2 }))

    await store.fetchMembers(TENANT_ID, 2)

    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/member?page=2&per_page=25`)
    expect(store.items).toHaveLength(1)
    expect(store.total).toBe(60)
    expect(store.page).toBe(2)
    expect(store.totalPages).toBe(3)
    expect(store.error).toBeNull()
  })

  it('fetchMembers failure surfaces the error and clears items', async () => {
    const store = useMembersStore()
    store.items = [mkMember()]
    mockApi.get.mockRejectedValueOnce(new Error('Not a member'))

    await store.fetchMembers(TENANT_ID)

    expect(store.error).toContain('Not a member')
    expect(store.items).toEqual([])
  })

  it('setMemberRole mirrors assign/unassign locally without duplicates', () => {
    const store = useMembersStore()
    store.items = [mkMember({ role_ids: ['r1'] })]

    store.setMemberRole('u1', 'r2', true)
    expect(store.items[0].role_ids).toEqual(['r1', 'r2'])

    // Idempotent add.
    store.setMemberRole('u1', 'r2', true)
    expect(store.items[0].role_ids).toEqual(['r1', 'r2'])

    store.setMemberRole('u1', 'r1', false)
    expect(store.items[0].role_ids).toEqual(['r2'])

    // Unknown user is a no-op, not a throw.
    store.setMemberRole('nobody', 'r1', true)
    expect(store.items[0].role_ids).toEqual(['r2'])
  })
})
