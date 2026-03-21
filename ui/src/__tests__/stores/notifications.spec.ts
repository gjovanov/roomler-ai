import { describe, it, expect, vi, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

import { useNotificationStore, type Notification } from '@/stores/notification'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

function makeNotification(overrides: Partial<Notification> = {}): Notification {
  return {
    id: 'n1',
    notification_type: 'message',
    title: 'New message',
    body: 'You have a new message',
    is_read: false,
    created_at: '2026-03-20T00:00:00Z',
    ...overrides,
  }
}

describe('useNotificationStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  describe('initial state', () => {
    it('should start with empty notifications and zero unread', () => {
      const store = useNotificationStore()
      expect(store.notifications).toEqual([])
      expect(store.unreadCount).toBe(0)
      expect(store.loading).toBe(false)
    })
  })

  describe('fetchNotifications', () => {
    it('should fetch and store notifications', async () => {
      const items = [makeNotification({ id: 'n1' }), makeNotification({ id: 'n2' })]
      mockApi.get.mockResolvedValueOnce({ items })

      const store = useNotificationStore()
      await store.fetchNotifications()

      expect(mockApi.get).toHaveBeenCalledWith('/notification')
      expect(store.notifications).toEqual(items)
      expect(store.loading).toBe(false)
    })

    it('should set loading while fetching', async () => {
      let resolvePromise: (v: unknown) => void
      mockApi.get.mockReturnValueOnce(new Promise((r) => { resolvePromise = r }))

      const store = useNotificationStore()
      const promise = store.fetchNotifications()
      expect(store.loading).toBe(true)

      resolvePromise!({ items: [] })
      await promise
      expect(store.loading).toBe(false)
    })

    it('should reset loading on error', async () => {
      mockApi.get.mockRejectedValueOnce(new Error('Network error'))

      const store = useNotificationStore()
      await expect(store.fetchNotifications()).rejects.toThrow()
      expect(store.loading).toBe(false)
    })
  })

  describe('fetchUnreadCount', () => {
    it('should fetch and set unread count', async () => {
      mockApi.get.mockResolvedValueOnce({ count: 5 })

      const store = useNotificationStore()
      await store.fetchUnreadCount()

      expect(mockApi.get).toHaveBeenCalledWith('/notification/unread-count')
      expect(store.unreadCount).toBe(5)
    })
  })

  describe('markRead', () => {
    it('should mark a notification as read and decrement unread count', async () => {
      mockApi.put.mockResolvedValueOnce({})

      const store = useNotificationStore()
      store.notifications = [makeNotification({ id: 'n1', is_read: false })]
      store.unreadCount = 3

      await store.markRead('n1')

      expect(mockApi.put).toHaveBeenCalledWith('/notification/n1/read')
      expect(store.notifications[0].is_read).toBe(true)
      expect(store.unreadCount).toBe(2)
    })

    it('should not go below zero for unread count', async () => {
      mockApi.put.mockResolvedValueOnce({})

      const store = useNotificationStore()
      store.notifications = [makeNotification({ id: 'n1', is_read: false })]
      store.unreadCount = 0

      await store.markRead('n1')
      expect(store.unreadCount).toBe(0)
    })

    it('should handle non-existent notification id gracefully', async () => {
      mockApi.put.mockResolvedValueOnce({})

      const store = useNotificationStore()
      store.notifications = [makeNotification({ id: 'n1' })]
      store.unreadCount = 1

      await store.markRead('nonexistent')
      expect(store.unreadCount).toBe(0)
    })
  })

  describe('markAllRead', () => {
    it('should mark all notifications as read and reset unread count', async () => {
      mockApi.post.mockResolvedValueOnce({})

      const store = useNotificationStore()
      store.notifications = [
        makeNotification({ id: 'n1', is_read: false }),
        makeNotification({ id: 'n2', is_read: false }),
      ]
      store.unreadCount = 2

      await store.markAllRead()

      expect(mockApi.post).toHaveBeenCalledWith('/notification/read-all')
      expect(store.notifications.every((n) => n.is_read)).toBe(true)
      expect(store.unreadCount).toBe(0)
    })
  })

  describe('addFromWs', () => {
    it('should prepend notification and increment unread count', () => {
      const store = useNotificationStore()
      store.notifications = [makeNotification({ id: 'n1' })]
      store.unreadCount = 1

      const newNotif = makeNotification({ id: 'n2', is_read: false })
      store.addFromWs(newNotif)

      expect(store.notifications[0].id).toBe('n2')
      expect(store.notifications.length).toBe(2)
      expect(store.unreadCount).toBe(2)
    })

    it('should not increment unread count for already-read notifications', () => {
      const store = useNotificationStore()
      store.unreadCount = 0

      store.addFromWs(makeNotification({ id: 'n1', is_read: true }))
      expect(store.unreadCount).toBe(0)
    })
  })

  describe('setUnreadCount', () => {
    it('should set the unread count directly', () => {
      const store = useNotificationStore()
      store.setUnreadCount(42)
      expect(store.unreadCount).toBe(42)
    })
  })
})
