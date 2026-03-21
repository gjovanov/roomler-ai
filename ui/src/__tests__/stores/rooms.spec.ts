import { describe, it, expect, vi, beforeEach } from 'vitest'
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
})
