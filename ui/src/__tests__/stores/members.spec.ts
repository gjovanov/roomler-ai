// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
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
    email: 'ada@memgrid.test',
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

// ─── FR-11 (#784): grid params + add-by-email + remove ───────────────────

describe('members store FR-11', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  it('fetchMembers builds q/sort/dir params from the opts form', async () => {
    const store = useMembersStore()
    mockApi.get.mockResolvedValueOnce(mkPage([]))
    await store.fetchMembers(TENANT_ID, { page: 3, perPage: 50, q: 'ada@x', sort: 'email', dir: 'desc' })
    expect(mockApi.get).toHaveBeenCalledWith(
      `/tenant/${TENANT_ID}/member?page=3&per_page=50&q=ada%40x&sort=email&dir=desc`,
    )
  })

  it('addByEmail posts the email form of POST /member', async () => {
    const store = useMembersStore()
    mockApi.post.mockResolvedValueOnce({ id: 'm9', user_id: 'u9', tenant_id: TENANT_ID })
    await store.addByEmail(TENANT_ID, 'new@corp.test')
    expect(mockApi.post).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/member`, {
      email: 'new@corp.test',
      role_ids: [],
    })
  })

  it('removeMember DELETEs the member row by user id', async () => {
    const store = useMembersStore()
    mockApi.delete.mockResolvedValueOnce({ removed: true })
    await store.removeMember(TENANT_ID, 'u2')
    expect(mockApi.delete).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/member/u2`)
  })

  it('a stale response never clobbers a newer one (seq guard)', async () => {
    const store = useMembersStore()
    let resolveSlow: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => { resolveSlow = r }))
    const slow = store.fetchMembers(TENANT_ID, { q: 'old' })

    mockApi.get.mockResolvedValueOnce(mkPage([mkMember({ id: 'fresh' })]))
    await store.fetchMembers(TENANT_ID, { q: 'new' })
    expect(store.items.map((m) => m.id)).toEqual(['fresh'])

    resolveSlow!(mkPage([mkMember({ id: 'stale' })]))
    await slow
    expect(store.items.map((m) => m.id)).toEqual(['fresh'])
  })
})
