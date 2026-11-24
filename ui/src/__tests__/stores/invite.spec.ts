// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4257 — the Invites page is reused when its org changes in place (a link,
// back or forward). The invite store records which org its list is of: a list
// for another org drops the previous org's invites at once, so none show
// while the new org's list loads or after that load failed, and a create or a
// batch answered for an org the page has left stays out of the list. Such a
// write still happened on the server: the call answers as before, with no
// error.
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import { useInviteStore } from '@/stores/invite'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

const ORG_A = 'a'.repeat(24)
const ORG_B = 'b'.repeat(24)

function invite(id: string, tenant: string, email: string) {
  return {
    id,
    code: `code-${id}`,
    tenant_id: tenant,
    inviter_id: 'u1',
    target_email: email,
    use_count: 0,
    status: 'active',
    assign_role_ids: [],
    created_at: '2026-11-20T00:00:00Z',
  }
}
const aInvite = invite('ia', ORG_A, 'guest@org-a.example')
const bInvite = invite('ib', ORG_B, 'guest@org-b.example')

function envelope(items: unknown[]) {
  return { items, total: items.length, page: 1, per_page: 25, total_pages: 1 }
}

describe('useInviteStore: the org its list is of (#4257)', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
    mockApi.get.mockReset()
    mockApi.post.mockReset()
  })

  it("a list for org B drops org A's invites at once, before it answers", async () => {
    const store = useInviteStore()
    mockApi.get.mockResolvedValueOnce(envelope([aInvite]))
    await store.listInvites(ORG_A)
    expect(store.invites.map((i) => i.id)).toEqual(['ia'])

    let answerB: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => { answerB = r }))
    const listing = store.listInvites(ORG_B)
    expect(store.invites).toEqual([])
    expect(store.total).toBe(0)
    answerB!(envelope([bInvite]))
    await listing
    expect(store.invites.map((i) => i.id)).toEqual(['ib'])
  })

  it("a failed list for org B leaves none of org A's invites", async () => {
    const store = useInviteStore()
    mockApi.get.mockResolvedValueOnce(envelope([aInvite]))
    await store.listInvites(ORG_A)

    mockApi.get.mockRejectedValueOnce(new Error('unavailable'))
    await expect(store.listInvites(ORG_B)).rejects.toThrow('unavailable')
    expect(store.invites).toEqual([])
    expect(store.error).toBe('unavailable')
  })

  it("an invite created for org A that answers after a list for org B stays out of org B's list, and still answers", async () => {
    const store = useInviteStore()
    mockApi.get.mockResolvedValueOnce(envelope([aInvite]))
    await store.listInvites(ORG_A)
    let answerCreate: (v: unknown) => void
    mockApi.post.mockReturnValueOnce(new Promise((r) => { answerCreate = r }))
    const creating = store.createInvite(ORG_A, { target_email: 'late@org-a.example' })

    mockApi.get.mockResolvedValueOnce(envelope([bInvite]))
    await store.listInvites(ORG_B)
    const late = invite('i-late', ORG_A, 'late@org-a.example')
    answerCreate!(late)
    await expect(creating).resolves.toEqual(late)
    expect(store.invites.map((i) => i.id)).toEqual(['ib'])
    expect(store.error).toBeNull()
  })

  it("a batch for org A that answers after a list for org B stays out of org B's list, and still answers", async () => {
    const store = useInviteStore()
    mockApi.get.mockResolvedValueOnce(envelope([aInvite]))
    await store.listInvites(ORG_A)
    let answerBatch: (v: unknown) => void
    mockApi.post.mockReturnValueOnce(new Promise((r) => { answerBatch = r }))
    const sending = store.batchCreateInvites(ORG_A, [{ target_email: 'late@org-a.example' }])

    mockApi.get.mockResolvedValueOnce(envelope([bInvite]))
    await store.listInvites(ORG_B)
    const late = invite('i-late', ORG_A, 'late@org-a.example')
    const answer = { results: [{ invite: late, target_email: 'late@org-a.example' }], created: 1, failed: 0 }
    answerBatch!(answer)
    await expect(sending).resolves.toEqual(answer)
    expect(store.invites.map((i) => i.id)).toEqual(['ib'])
    expect(store.error).toBeNull()
  })

  it("the same org's invites keep the list: its next page drops nothing while it loads, and a create joins it", async () => {
    const store = useInviteStore()
    mockApi.get.mockResolvedValueOnce(envelope([aInvite]))
    await store.listInvites(ORG_A)

    let answerPage2: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => { answerPage2 = r }))
    const listing = store.listInvites(ORG_A, { page: 2 })
    expect(store.invites.map((i) => i.id)).toEqual(['ia'])
    answerPage2!(envelope([invite('ia2', ORG_A, 'second@org-a.example')]))
    await listing

    mockApi.post.mockResolvedValueOnce(invite('ia3', ORG_A, 'third@org-a.example'))
    await store.createInvite(ORG_A, { target_email: 'third@org-a.example' })
    expect(store.invites.map((i) => i.id)).toEqual(['ia3', 'ia2'])
  })

  // #4257 (the review's F3) — a create or a batch set the page's spinner and
  // its error with no look at the org. One for org A answered after the list
  // moved to org B took org B's spinner away while org B's list loaded, and a
  // failed one showed org A's error above org B's grid. Such an answer leaves
  // both alone, and a write never ends a list fetch's spinner.
  type Store = ReturnType<typeof useInviteStore>
  const writes: Array<{ name: string; run: (s: Store, t: string) => Promise<unknown>; answer: () => unknown }> = [
    {
      name: 'a create',
      run: (s, t) => s.createInvite(t, { target_email: 'new@org-a.example' }),
      answer: () => invite('ic', ORG_A, 'new@org-a.example'),
    },
    {
      name: 'a batch',
      run: (s, t) => s.batchCreateInvites(t, [{ target_email: 'new@org-a.example' }]),
      answer: () => ({ results: [], created: 0, failed: 0 }),
    },
  ]
  for (const write of writes) {
    it(`${write.name} for org A answered after a list for org B leaves org B's spinner on while that list loads`, async () => {
      const store = useInviteStore()
      let answerWrite!: (v: unknown) => void
      mockApi.post.mockReturnValueOnce(new Promise((r) => (answerWrite = r)) as never)
      const writing = write.run(store, ORG_A)
      let answerB!: (v: unknown) => void
      mockApi.get.mockReturnValueOnce(new Promise((r) => (answerB = r)) as never)
      const listing = store.listInvites(ORG_B)

      answerWrite(write.answer())
      await writing
      expect(store.loading).toBe(true)
      answerB(envelope([bInvite]))
      await listing
      expect(store.loading).toBe(false)
    })

    it(`${write.name} for org A that fails after a list for org B shows no error over org B's list`, async () => {
      const store = useInviteStore()
      let failWrite!: (e: unknown) => void
      mockApi.post.mockReturnValueOnce(new Promise((_, rej) => (failWrite = rej)) as never)
      const writing = write.run(store, ORG_A).catch((e: Error) => e.message)
      mockApi.get.mockResolvedValueOnce(envelope([bInvite]))
      await store.listInvites(ORG_B)

      failWrite(new Error("org A's refusal"))
      expect(await writing).toBe("org A's refusal")
      expect(store.error).toBeNull()
      expect(store.loading).toBe(false)
    })
  }

  it("a create for org A never ends the spinner of org A's own list fetch", async () => {
    const store = useInviteStore()
    mockApi.get.mockResolvedValueOnce(envelope([aInvite]))
    await store.listInvites(ORG_A)
    let answerList!: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => (answerList = r)) as never)
    const listing = store.listInvites(ORG_A, { page: 2 })

    mockApi.post.mockResolvedValueOnce(invite('ic', ORG_A, 'new@org-a.example'))
    await store.createInvite(ORG_A, { target_email: 'new@org-a.example' })
    expect(store.loading).toBe(true)
    answerList(envelope([aInvite]))
    await listing
    expect(store.loading).toBe(false)
  })
})
