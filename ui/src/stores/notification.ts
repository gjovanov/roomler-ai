import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

export interface Notification {
  id: string
  notification_type: string
  title: string
  body: string
  link?: string
  is_read: boolean
  created_at: string
}

export const useNotificationStore = defineStore('notifications', () => {
  const notifications = ref<Notification[]>([])
  const unreadCount = ref(0)
  const loading = ref(false)

  async function fetchNotifications() {
    loading.value = true
    try {
      const data = await api.get<{ items: Notification[] }>('/notification')
      notifications.value = data.items
    } finally {
      loading.value = false
    }
  }

  async function fetchUnreadCount() {
    const data = await api.get<{ count: number }>('/notification/unread-count')
    unreadCount.value = data.count
  }

  async function markRead(notificationId: string) {
    await api.put(`/notification/${notificationId}/read`)
    const idx = notifications.value.findIndex((n) => n.id === notificationId)
    if (idx !== -1) {
      notifications.value[idx].is_read = true
    }
    unreadCount.value = Math.max(0, unreadCount.value - 1)
  }

  async function markAllRead() {
    await api.post('/notification/read-all')
    notifications.value.forEach((n) => { n.is_read = true })
    unreadCount.value = 0
  }

  function addFromWs(notification: Notification) {
    notifications.value.unshift(notification)
    if (!notification.is_read) {
      unreadCount.value++
    }
  }

  function setUnreadCount(count: number) {
    unreadCount.value = count
  }

  return {
    notifications,
    unreadCount,
    loading,
    fetchNotifications,
    fetchUnreadCount,
    markRead,
    markAllRead,
    addFromWs,
    setUnreadCount,
  }
})
