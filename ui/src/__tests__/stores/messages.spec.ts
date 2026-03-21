import { describe, it, expect, vi, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

// Mock router (needed by auth store dependency)
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
  },
}))

import { useMessageStore } from '@/stores/messages'
import { useAuthStore } from '@/stores/auth'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

function makeMessage(overrides: Partial<{
  id: string
  room_id: string
  author_id: string
  author_name: string
  content: string
  thread_id: string
  is_thread_root: boolean
  is_pinned: boolean
  is_edited: boolean
  reaction_summary: { emoji: string; count: number }[]
  attachments: unknown[]
  created_at: string
  updated_at: string
}> = {}) {
  return {
    id: 'msg-1',
    room_id: 'room-1',
    author_id: 'user-1',
    author_name: 'User One',
    content: 'Hello',
    is_thread_root: false,
    is_pinned: false,
    is_edited: false,
    reaction_summary: [],
    attachments: [],
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
    ...overrides,
  }
}

describe('useMessageStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  describe('fetchMessages', () => {
    it('should populate the messages array from API', async () => {
      const msgs = [makeMessage({ id: 'a' }), makeMessage({ id: 'b' })]
      mockApi.get.mockResolvedValueOnce({ items: msgs, total: 2 })

      const store = useMessageStore()
      await store.fetchMessages('t1', 'r1')

      expect(mockApi.get).toHaveBeenCalledWith('/tenant/t1/room/r1/message?per_page=25')
      expect(store.messages).toEqual(msgs)
      expect(store.loading).toBe(false)
    })

    it('should set hasMore to true when total exceeds returned items', async () => {
      mockApi.get.mockResolvedValueOnce({ items: [makeMessage()], total: 50 })

      const store = useMessageStore()
      await store.fetchMessages('t1', 'r1')

      expect(store.hasMore).toBe(true)
    })

    it('should set hasMore to false when all items returned', async () => {
      const msgs = [makeMessage({ id: 'a' })]
      mockApi.get.mockResolvedValueOnce({ items: msgs, total: 1 })

      const store = useMessageStore()
      await store.fetchMessages('t1', 'r1')

      expect(store.hasMore).toBe(false)
    })
  })

  describe('addMessageFromWs', () => {
    it('should add a message to messages list', () => {
      const store = useMessageStore()
      const msg = makeMessage({ id: 'ws-1' })

      store.addMessageFromWs(msg as never)

      expect(store.messages).toHaveLength(1)
      expect(store.messages[0].id).toBe('ws-1')
    })

    it('should deduplicate messages with the same id', () => {
      const store = useMessageStore()
      const msg = makeMessage({ id: 'ws-1' })

      store.addMessageFromWs(msg as never)
      store.addMessageFromWs(msg as never)

      expect(store.messages).toHaveLength(1)
    })

    it('should add thread messages to threadMessages when thread_id is set', () => {
      const store = useMessageStore()
      const msg = makeMessage({ id: 'thread-msg', thread_id: 'parent-1' })

      store.addMessageFromWs(msg as never)

      expect(store.messages).toHaveLength(0)
      expect(store.threadMessages).toHaveLength(1)
      expect(store.threadMessages[0].id).toBe('thread-msg')
    })

    it('should deduplicate thread messages', () => {
      const store = useMessageStore()
      const msg = makeMessage({ id: 'thread-msg', thread_id: 'parent-1' })

      store.addMessageFromWs(msg as never)
      store.addMessageFromWs(msg as never)

      expect(store.threadMessages).toHaveLength(1)
    })
  })

  describe('addReaction', () => {
    it('should optimistically increment reaction count for existing emoji', async () => {
      mockApi.post.mockResolvedValueOnce({})

      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1', reaction_summary: [{ emoji: '👍', count: 2 }] })] as never

      await store.addReaction('t1', 'r1', 'm1', '👍')

      expect(store.messages[0].reaction_summary[0].count).toBe(3)
    })

    it('should optimistically add new emoji reaction', async () => {
      mockApi.post.mockResolvedValueOnce({})

      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1', reaction_summary: [] })] as never

      await store.addReaction('t1', 'r1', 'm1', '🎉')

      expect(store.messages[0].reaction_summary).toEqual([{ emoji: '🎉', count: 1 }])
    })

    it('should track user reaction locally', async () => {
      mockApi.post.mockResolvedValueOnce({})

      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1' })] as never

      await store.addReaction('t1', 'r1', 'm1', '❤️')

      expect(store.hasUserReacted('m1', '❤️')).toBe(true)
    })

    it('should call the API with correct endpoint', async () => {
      mockApi.post.mockResolvedValueOnce({})

      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1' })] as never

      await store.addReaction('t1', 'r1', 'm1', '👍')

      expect(mockApi.post).toHaveBeenCalledWith(
        '/tenant/t1/room/r1/message/m1/reaction',
        { emoji: '👍' },
      )
    })
  })

  describe('removeReaction', () => {
    it('should optimistically decrement reaction count', async () => {
      mockApi.delete.mockResolvedValueOnce({})

      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1', reaction_summary: [{ emoji: '👍', count: 3 }] })] as never

      await store.removeReaction('t1', 'r1', 'm1', '👍')

      expect(store.messages[0].reaction_summary[0].count).toBe(2)
    })

    it('should remove emoji entry when count reaches zero', async () => {
      mockApi.delete.mockResolvedValueOnce({})

      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1', reaction_summary: [{ emoji: '👍', count: 1 }] })] as never

      await store.removeReaction('t1', 'r1', 'm1', '👍')

      expect(store.messages[0].reaction_summary).toEqual([])
    })

    it('should remove user reaction tracking', async () => {
      mockApi.post.mockResolvedValueOnce({})
      mockApi.delete.mockResolvedValueOnce({})

      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1', reaction_summary: [{ emoji: '❤️', count: 1 }] })] as never

      await store.addReaction('t1', 'r1', 'm1', '❤️')
      expect(store.hasUserReacted('m1', '❤️')).toBe(true)

      await store.removeReaction('t1', 'r1', 'm1', '❤️')
      expect(store.hasUserReacted('m1', '❤️')).toBe(false)
    })
  })

  describe('handleReactionFromWs', () => {
    it('should add reaction from another user', () => {
      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1', reaction_summary: [] })] as never

      // Set up auth store with a different user
      const authStore = useAuthStore()
      authStore.user = { id: 'me', email: '', username: '', display_name: '' } as never

      store.handleReactionFromWs({ action: 'add', message_id: 'm1', emoji: '🔥', user_id: 'other-user' })

      expect(store.messages[0].reaction_summary).toEqual([{ emoji: '🔥', count: 1 }])
    })

    it('should skip reaction from current user (already optimistically updated)', () => {
      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1', reaction_summary: [{ emoji: '👍', count: 1 }] })] as never

      const authStore = useAuthStore()
      authStore.user = { id: 'me', email: '', username: '', display_name: '' } as never

      store.handleReactionFromWs({ action: 'add', message_id: 'm1', emoji: '👍', user_id: 'me' })

      // Count should remain 1, not 2
      expect(store.messages[0].reaction_summary[0].count).toBe(1)
    })

    it('should remove reaction from another user via WS', () => {
      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1', reaction_summary: [{ emoji: '👍', count: 1 }] })] as never

      const authStore = useAuthStore()
      authStore.user = { id: 'me', email: '', username: '', display_name: '' } as never

      store.handleReactionFromWs({ action: 'remove', message_id: 'm1', emoji: '👍', user_id: 'other' })

      expect(store.messages[0].reaction_summary).toEqual([])
    })
  })

  describe('updateMessageFromWs', () => {
    it('should update an existing message', () => {
      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1', content: 'old' })] as never

      const updated = makeMessage({ id: 'm1', content: 'new' })
      store.updateMessageFromWs(updated as never)

      expect(store.messages[0].content).toBe('new')
    })
  })

  describe('removeMessageFromWs', () => {
    it('should remove a message by id', () => {
      const store = useMessageStore()
      store.messages = [makeMessage({ id: 'm1' }), makeMessage({ id: 'm2' })] as never

      store.removeMessageFromWs({ id: 'm1', room_id: 'r1' })

      expect(store.messages).toHaveLength(1)
      expect(store.messages[0].id).toBe('m2')
    })
  })
})
