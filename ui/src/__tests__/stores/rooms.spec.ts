// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

vi.mock('@/plugins/router', () => ({
  default: { push: vi.fn() },
}))

vi.mock('@/composables/usePush', () => ({
  subscribePush: vi.fn(() => Promise.resolve()),
  unsubscribePush: vi.fn(() => Promise.resolve()),
}))

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
    upload: vi.fn(),
  },
}))

import { useRoomStore, type Room } from '@/stores/rooms'
import { useWsStore } from '@/stores/ws'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

function makeRoom(overrides: Partial<Room> = {}): Room {
  return {
    id: 'room-1',
    tenant_id: 't1',
    name: 'General',
    path: '/general',
    is_open: true,
    is_archived: false,
    is_read_only: false,
    is_default: false,
    has_media: false,
    participant_count: 0,
    member_count: 5,
    message_count: 100,
    created_at: '2026-01-01T00:00:00Z',
    ...overrides,
  }
}

describe('useRoomStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  describe('setCurrent', () => {
    it('should update current room', () => {
      const store = useRoomStore()
      const room = makeRoom({ id: 'r1', name: 'Test' })

      store.setCurrent(room)

      expect(store.current).toEqual(room)
    })

    it('should allow setting current to null', () => {
      const store = useRoomStore()
      store.setCurrent(makeRoom())
      store.setCurrent(null)

      expect(store.current).toBeNull()
    })
  })

  describe('fetchRooms', () => {
    it('should populate rooms array from API', async () => {
      const rooms = [makeRoom({ id: 'r1' }), makeRoom({ id: 'r2' })]
      mockApi.get.mockResolvedValueOnce(rooms)

      const store = useRoomStore()
      await store.fetchRooms('t1')

      expect(mockApi.get).toHaveBeenCalledWith('/tenant/t1/room')
      expect(store.rooms).toEqual(rooms)
      expect(store.loading).toBe(false)
    })

    it('should set loading during fetch', async () => {
      mockApi.get.mockResolvedValueOnce([])

      const store = useRoomStore()
      const promise = store.fetchRooms('t1')
      expect(store.loading).toBe(true)
      await promise
      expect(store.loading).toBe(false)
    })

    it('should reset loading even on error', async () => {
      mockApi.get.mockRejectedValueOnce(new Error('fail'))

      const store = useRoomStore()
      await expect(store.fetchRooms('t1')).rejects.toThrow('fail')
      expect(store.loading).toBe(false)
    })
  })

  describe('tree and rootRooms', () => {
    it('should compute root rooms (no parent_id)', () => {
      const store = useRoomStore()
      store.rooms = [
        makeRoom({ id: 'r1', parent_id: undefined }),
        makeRoom({ id: 'r2', parent_id: 'r1' }),
        makeRoom({ id: 'r3', parent_id: undefined }),
      ] as Room[]

      expect(store.rootRooms.map(r => r.id)).toEqual(['r1', 'r3'])
    })

    it('should return children of a parent room', () => {
      const store = useRoomStore()
      store.rooms = [
        makeRoom({ id: 'parent', parent_id: undefined }),
        makeRoom({ id: 'child1', parent_id: 'parent' }),
        makeRoom({ id: 'child2', parent_id: 'parent' }),
        makeRoom({ id: 'other', parent_id: undefined }),
      ] as Room[]

      const children = store.childrenOf('parent')
      expect(children.map(r => r.id)).toEqual(['child1', 'child2'])
    })

    it('should return empty array for rooms with no children', () => {
      const store = useRoomStore()
      store.rooms = [makeRoom({ id: 'r1' })] as Room[]

      expect(store.childrenOf('r1')).toEqual([])
    })
  })

  describe('unread counts', () => {
    it('should track unread counts per room', () => {
      const store = useRoomStore()

      store.incrementUnread('room-a')
      store.incrementUnread('room-a')
      store.incrementUnread('room-b')

      expect(store.unreadCounts['room-a']).toBe(2)
      expect(store.unreadCounts['room-b']).toBe(1)
    })

    it('should compute totalUnread across all rooms', () => {
      const store = useRoomStore()

      store.incrementUnread('room-a')
      store.incrementUnread('room-a')
      store.incrementUnread('room-b')

      expect(store.totalUnread).toBe(3)
    })

    it('should return 0 totalUnread when no unreads', () => {
      const store = useRoomStore()
      expect(store.totalUnread).toBe(0)
    })

    it('should fetch unread count from API', async () => {
      mockApi.get.mockResolvedValueOnce({ count: 7 })

      const store = useRoomStore()
      await store.fetchUnreadCount('t1', 'r1')

      expect(mockApi.get).toHaveBeenCalledWith('/tenant/t1/room/r1/message/unread-count')
      expect(store.unreadCounts['r1']).toBe(7)
    })

    it('should call API on markMessagesRead', async () => {
      mockApi.post.mockResolvedValueOnce({})

      const store = useRoomStore()
      store.unreadCounts['r1'] = 5

      await store.markMessagesRead('t1', 'r1', ['m1', 'm2'])

      expect(mockApi.post).toHaveBeenCalledWith('/tenant/t1/room/r1/message/read', {
        message_ids: ['m1', 'm2'],
      })
    })

    it('should preserve unread count after markMessagesRead (caller fetches accurate count)', async () => {
      mockApi.post.mockResolvedValueOnce({})

      const store = useRoomStore()
      store.unreadCounts['r1'] = 5

      await store.markMessagesRead('t1', 'r1', ['m1', 'm2', 'm3'])

      // markMessagesRead no longer decrements locally; caller is responsible for fetching accurate count
      expect(store.unreadCounts['r1']).toBe(5)
    })

    it('should not call API for empty messageIds', async () => {
      const store = useRoomStore()
      await store.markMessagesRead('t1', 'r1', [])
      expect(mockApi.post).not.toHaveBeenCalled()
    })
  })

  describe('updateRoomCallStatus', () => {
    it('should update conference status on a room in the list', () => {
      const store = useRoomStore()
      store.rooms = [makeRoom({ id: 'r1', conference_status: undefined, participant_count: 0 })] as Room[]

      store.updateRoomCallStatus('r1', 'in_progress', 3)

      expect(store.rooms[0].conference_status).toBe('in_progress')
      expect(store.rooms[0].participant_count).toBe(3)
    })

    it('should update current room if it matches', () => {
      const store = useRoomStore()
      const room = makeRoom({ id: 'r1' })
      store.rooms = [room] as Room[]
      store.setCurrent(room)

      store.updateRoomCallStatus('r1', 'in_progress', 2)

      expect(store.current!.conference_status).toBe('in_progress')
      expect(store.current!.participant_count).toBe(2)
    })

    it('should clear conference status when null is passed', () => {
      const store = useRoomStore()
      store.rooms = [makeRoom({ id: 'r1', conference_status: 'in_progress' })] as Room[]

      store.updateRoomCallStatus('r1', null, 0)

      expect(store.rooms[0].conference_status).toBeUndefined()
    })
  })

  describe('createRoom', () => {
    it('should add the new room to the rooms list', async () => {
      const newRoom = makeRoom({ id: 'new-room', name: 'New' })
      mockApi.post.mockResolvedValueOnce(newRoom)

      const store = useRoomStore()
      const result = await store.createRoom('t1', { name: 'New', is_open: true })

      expect(result).toEqual(newRoom)
      expect(store.rooms).toContainEqual(newRoom)
    })
  })

  // #3416 — a join sent before the page's socket had its id opened a call
  // session no leave from the page could close: the leave names its
  // connection. The join waits for the id, up to a bound, and is refused past
  // it.
  describe('joinCall waits for the socket id', () => {
    const joinPath = '/tenant/t1/room/r1/call/join'

    afterEach(() => {
      vi.useRealTimers()
    })

    it('joins naming the id the socket already has', async () => {
      useWsStore().connectionId = 'conn-1'

      await useRoomStore().joinCall('t1', 'r1')

      expect(mockApi.post).toHaveBeenCalledWith(joinPath, { connection_id: 'conn-1' })
    })

    it('holds the join until the socket has its id, then names it', async () => {
      vi.useFakeTimers()
      const joining = useRoomStore().joinCall('t1', 'r1')

      await vi.advanceTimersByTimeAsync(9_000)
      expect(mockApi.post, 'the join went out before the socket had its id').not.toHaveBeenCalled()

      useWsStore().connectionId = 'conn-2'
      await joining

      expect(mockApi.post).toHaveBeenCalledTimes(1)
      expect(mockApi.post).toHaveBeenCalledWith(joinPath, { connection_id: 'conn-2' })
    })

    it('is refused past the bound with a reason the person can read, and sends nothing', async () => {
      vi.useFakeTimers()
      const joining = useRoomStore().joinCall('t1', 'r1')
      const refused = expect(joining).rejects.toThrow('Not connected to the server')

      await vi.advanceTimersByTimeAsync(10_000)

      await refused
      expect(mockApi.post).not.toHaveBeenCalled()
    })

    it('stops waiting once refused: an id that comes later sends no join', async () => {
      vi.useFakeTimers()
      const joining = useRoomStore().joinCall('t1', 'r1').catch(() => undefined)
      await vi.advanceTimersByTimeAsync(10_000)
      await joining

      useWsStore().connectionId = 'conn-3'
      await vi.advanceTimersByTimeAsync(0)

      expect(mockApi.post).not.toHaveBeenCalled()
    })
  })
})

// #4257 — the store holds one org's rooms at a time, and knows which. The
// shell reloads them on every change of org (AppLayout); a page that only
// lists or picks them asks with `ensureRooms`, which fetches only when the
// store does not hold that org's rooms.
describe('useRoomStore: one org at a time (#4257)', () => {
  const ORG_A = 'org_a'
  const ORG_B = 'org_b'
  const ids = () => useRoomStore().rooms.map((r) => r.id)
  const asked = () => mockApi.get.mock.calls.map((c) => c[0])
  /** Let every answer already handed over land. */
  const settle = () => new Promise((r) => setTimeout(r, 0))
  /** The next GET answers when the test says so. */
  function heldGet() {
    let resolve!: (v: unknown) => void
    let reject!: (e: unknown) => void
    mockApi.get.mockReturnValueOnce(
      new Promise((res, rej) => {
        resolve = res
        reject = rej
      }) as never,
    )
    return { resolve, reject }
  }

  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
    // A test that stops before its held GET is sent must not hand it to the next.
    mockApi.get.mockReset()
  })

  it("a fetch for org B drops org A's rooms at once, and org A's late answer", async () => {
    const s = useRoomStore()
    mockApi.get.mockResolvedValueOnce([makeRoom({ id: 'ra', tenant_id: ORG_A })])
    await s.fetchRooms(ORG_A)
    const lateA = heldGet()
    const refreshA = s.fetchRooms(ORG_A)
    const answerB = heldGet()
    const loadB = s.fetchRooms(ORG_B)
    expect(s.rooms).toEqual([])

    answerB.resolve([makeRoom({ id: 'rb', tenant_id: ORG_B })])
    await loadB
    lateA.resolve([makeRoom({ id: 'ra' }), makeRoom({ id: 'ra2' })])
    await refreshA
    expect(ids()).toEqual(['rb'])
    expect(s.loading).toBe(false)
  })

  it("org A's late failure does not end org B's load", async () => {
    const s = useRoomStore()
    const failA = heldGet()
    const loadA = s.fetchRooms(ORG_A).catch(() => undefined)
    const answerB = heldGet()
    const loadB = s.fetchRooms(ORG_B)
    failA.reject(new Error('Missing permission'))
    await settle()
    expect(s.loading).toBe(true)

    answerB.resolve([makeRoom({ id: 'rb', tenant_id: ORG_B })])
    await loadB
    await loadA
    expect(ids()).toEqual(['rb'])
    expect(s.loading).toBe(false)
  })

  it("a room created for org A once the list is org B's stays out of it", async () => {
    const s = useRoomStore()
    mockApi.get.mockResolvedValueOnce([makeRoom({ id: 'ra', tenant_id: ORG_A })])
    await s.fetchRooms(ORG_A)
    let created!: (v: unknown) => void
    mockApi.post.mockReturnValueOnce(new Promise((r) => (created = r)) as never)
    const create = s.createRoom(ORG_A, { name: 'New' })
    mockApi.get.mockResolvedValueOnce([makeRoom({ id: 'rb', tenant_id: ORG_B })])
    await s.fetchRooms(ORG_B)
    created(makeRoom({ id: 'ra-new', tenant_id: ORG_A }))
    await create
    expect(ids()).toEqual(['rb'])
  })

  // The welcome tour counts the rooms right after its await (AppLayout). A
  // fetch whose own answer was dropped must not settle before the newest one
  // has landed: the tour would count an empty list, and could send a new user
  // to the tutorial in an org that has rooms.
  it('a fetch superseded by a newer one for the same org settles once the newer one has landed', async () => {
    const s = useRoomStore()
    const first = heldGet()
    let settled = false
    const tour = s.fetchRooms(ORG_A).then(() => {
      settled = true
      return ids()
    })
    const second = heldGet()
    const shell = s.fetchRooms(ORG_A)
    first.resolve([makeRoom({ id: 'r1' })])
    await settle()
    expect(settled).toBe(false)

    second.resolve([makeRoom({ id: 'r1' }), makeRoom({ id: 'r2' })])
    await shell
    expect(await tour).toEqual(['r1', 'r2'])
  })

  it("ensureRooms for org B while holding org A's: one fetch, for org B", async () => {
    const s = useRoomStore()
    mockApi.get.mockResolvedValueOnce([makeRoom({ id: 'ra', tenant_id: ORG_A })])
    await s.fetchRooms(ORG_A)
    mockApi.get.mockResolvedValueOnce([makeRoom({ id: 'rb', tenant_id: ORG_B })])
    await s.ensureRooms(ORG_B)
    expect(asked()).toEqual([`/tenant/${ORG_A}/room`, `/tenant/${ORG_B}/room`])
    expect(ids()).toEqual(['rb'])
    expect(s.roomsTenantId).toBe(ORG_B)
  })

  it("ensureRooms for org A while holding org A's: no fetch, for an org with no rooms too", async () => {
    const s = useRoomStore()
    mockApi.get.mockResolvedValueOnce([])
    await s.fetchRooms(ORG_A)
    await s.ensureRooms(ORG_A)
    await s.ensureRooms(ORG_A)
    expect(asked()).toEqual([`/tenant/${ORG_A}/room`])
  })

  it("ensureRooms while org A's fetch is in flight sends no second GET, and settles once org A's rooms have landed", async () => {
    const s = useRoomStore()
    const answer = heldGet()
    const loading = s.fetchRooms(ORG_A)
    let settled = false
    const ensured = s.ensureRooms(ORG_A).then(() => {
      settled = true
      return ids()
    })
    await settle()
    expect(settled).toBe(false)

    answer.resolve([makeRoom({ id: 'ra', tenant_id: ORG_A })])
    expect(await ensured).toEqual(['ra'])
    await loading
    expect(asked()).toEqual([`/tenant/${ORG_A}/room`])
  })

  it("ensureRooms while org A's fetch is in flight fails with it, so a count after it never reads a list that did not load", async () => {
    const s = useRoomStore()
    const answer = heldGet()
    const loading = s.fetchRooms(ORG_A).catch(() => undefined)
    const ensured = s.ensureRooms(ORG_A)
    answer.reject(new Error('fail'))
    await expect(ensured).rejects.toThrow('fail')
    await loading
  })

  it('ensureRooms after a failed fetch: fetches', async () => {
    const s = useRoomStore()
    mockApi.get.mockRejectedValueOnce(new Error('network'))
    await s.fetchRooms(ORG_A).catch(() => undefined)
    mockApi.get.mockResolvedValueOnce([makeRoom({ id: 'ra', tenant_id: ORG_A })])
    await s.ensureRooms(ORG_A)
    expect(asked()).toHaveLength(2)
    expect(ids()).toEqual(['ra'])
  })

  // #4257 (the review's F5) — the shell counts the unread messages of the rooms
  // it fetched (`AppLayout.vue`: `fetchRooms(tid).then(() =>
  // fetchAllUnreadCounts(tid))`). A superseded fetch settles with the newest
  // one, so its count ran for the previous org over the new org's rooms: one
  // request a room, each refused. A count runs over its own org's rooms only.
  it('fetchAllUnreadCounts for an org the rooms are not of asks nothing', async () => {
    const s = useRoomStore()
    mockApi.get.mockResolvedValueOnce([makeRoom({ id: 'rb', tenant_id: ORG_B })])
    await s.fetchRooms(ORG_B)
    mockApi.get.mockClear()
    await s.fetchAllUnreadCounts(ORG_A)
    await settle()
    expect(asked()).toEqual([])
  })

  it("the shell's count after a fetch that the next org's superseded asks nothing for the previous org, and the next org's count runs", async () => {
    const s = useRoomStore()
    const answerA = heldGet()
    const chainA = s.fetchRooms(ORG_A).then(() => s.fetchAllUnreadCounts(ORG_A))
    const answerB = heldGet()
    const chainB = s.fetchRooms(ORG_B).then(() => s.fetchAllUnreadCounts(ORG_B))
    // Every later GET is an unread count.
    mockApi.get.mockResolvedValue({ count: 3 })
    answerA.resolve([makeRoom({ id: 'ra', tenant_id: ORG_A })])
    answerB.resolve([makeRoom({ id: 'rb', tenant_id: ORG_B })])
    await Promise.all([chainA, chainB])
    await settle()
    expect(asked().filter((p) => p.startsWith(`/tenant/${ORG_A}/room/`))).toEqual([])
    expect(asked()).toContain(`/tenant/${ORG_B}/room/rb/message/unread-count`)
    expect(s.unreadCounts.rb).toBe(3)
  })
})

// #4332 — the current room is one org's too, and the store records which. The
// API's room carries no org (`Room.tenant_id` is declared, never sent), so a
// page that names the current room under an org (the Files page) asks
// `currentTenantId`, never the room. Nothing clears `current` on a change of
// org: a room opened in org A's chat is still current under org B.
describe('useRoomStore: the current room records its org (#4332)', () => {
  const ORG_A = 'org_a'
  const ORG_B = 'org_b'
  /** Let every answer already handed over land. */
  const settle = () => new Promise((r) => setTimeout(r, 0))

  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
    mockApi.get.mockReset()
  })

  it('fetchRoom records the org it asked with the room, once the answer has landed: until then the previous room keeps its own', async () => {
    const s = useRoomStore()
    // The answer's own `tenant_id` ('t1') is not the org: the store records the org it asked.
    mockApi.get.mockResolvedValueOnce(makeRoom({ id: 'ra' }))
    await s.fetchRoom(ORG_A, 'ra')
    expect.soft(s.current?.id).toBe('ra')
    expect.soft(s.currentTenantId).toBe(ORG_A)

    let answerB!: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => (answerB = r)) as never)
    const loadB = s.fetchRoom(ORG_B, 'rb')
    await settle()
    expect.soft(s.current?.id, 'the previous room until the answer lands').toBe('ra')
    expect.soft(s.currentTenantId, "the previous room's org until the answer lands").toBe(ORG_A)

    answerB(makeRoom({ id: 'rb' }))
    await loadB
    expect.soft(s.current?.id).toBe('rb')
    expect.soft(s.currentTenantId).toBe(ORG_B)
  })

  it('a fetchRoom that fails leaves the current room and its org', async () => {
    const s = useRoomStore()
    mockApi.get.mockResolvedValueOnce(makeRoom({ id: 'ra' }))
    await s.fetchRoom(ORG_A, 'ra')
    mockApi.get.mockRejectedValueOnce(new Error('Not found'))
    await expect(s.fetchRoom(ORG_B, 'rb')).rejects.toThrow('Not found')
    expect.soft(s.current?.id).toBe('ra')
    expect.soft(s.currentTenantId).toBe(ORG_A)
  })

  it('setCurrent records the org it is given, and none without one', () => {
    const s = useRoomStore()
    s.setCurrent(makeRoom({ id: 'ra' }), ORG_A)
    expect.soft(s.current?.id).toBe('ra')
    expect.soft(s.currentTenantId).toBe(ORG_A)

    s.setCurrent(makeRoom({ id: 'rb' }))
    expect.soft(s.current?.id).toBe('rb')
    expect.soft(s.currentTenantId, 'a room set with no org is no org').toBeNull()
  })

  it('setCurrent(null) clears the room and its org', () => {
    const s = useRoomStore()
    s.setCurrent(makeRoom({ id: 'ra' }), ORG_A)
    s.setCurrent(null)
    expect.soft(s.current).toBeNull()
    expect.soft(s.currentTenantId).toBeNull()
  })

  it('deleteRoom of the current room clears the room and its org; of another room, neither', async () => {
    const s = useRoomStore()
    mockApi.get.mockResolvedValueOnce(makeRoom({ id: 'ra' }))
    await s.fetchRoom(ORG_A, 'ra')
    mockApi.delete.mockResolvedValue(undefined as never)

    await s.deleteRoom(ORG_A, 'other')
    expect.soft(s.current?.id).toBe('ra')
    expect.soft(s.currentTenantId).toBe(ORG_A)

    await s.deleteRoom(ORG_A, 'ra')
    expect.soft(s.current).toBeNull()
    expect.soft(s.currentTenantId).toBeNull()
  })
})
