import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

interface Reaction {
  emoji: string
  count: number
}

interface Message {
  id: string
  channel_id: string
  author_id: string
  author_name?: string
  content: string
  thread_id?: string
  is_thread_root: boolean
  is_pinned: boolean
  reaction_summary: Reaction[]
  created_at: string
  updated_at: string
}

export const useMessageStore = defineStore('messages', () => {
  const messages = ref<Message[]>([])
  const threadMessages = ref<Message[]>([])
  const loading = ref(false)

  async function fetchMessages(tenantId: string, channelId: string) {
    loading.value = true
    try {
      const data = await api.get<{ items: Message[] }>(
        `/tenant/${tenantId}/channel/${channelId}/message`,
      )
      messages.value = data.items
    } finally {
      loading.value = false
    }
  }

  async function sendMessage(tenantId: string, channelId: string, content: string, threadId?: string) {
    const msg = await api.post<Message>(
      `/tenant/${tenantId}/channel/${channelId}/message`,
      { content, thread_id: threadId },
    )
    if (threadId) {
      threadMessages.value.push(msg)
    } else {
      messages.value.push(msg)
    }
    return msg
  }

  async function editMessage(tenantId: string, channelId: string, messageId: string, content: string) {
    const msg = await api.put<Message>(
      `/tenant/${tenantId}/channel/${channelId}/message/${messageId}`,
      { content },
    )
    const idx = messages.value.findIndex((m) => m.id === messageId)
    if (idx !== -1) messages.value[idx] = msg
    return msg
  }

  async function deleteMessage(tenantId: string, channelId: string, messageId: string) {
    await api.delete(`/tenant/${tenantId}/channel/${channelId}/message/${messageId}`)
    messages.value = messages.value.filter((m) => m.id !== messageId)
  }

  async function addReaction(tenantId: string, channelId: string, messageId: string, emoji: string) {
    await api.post(`/tenant/${tenantId}/channel/${channelId}/message/${messageId}/reaction`, {
      emoji,
    })
  }

  async function togglePin(tenantId: string, channelId: string, messageId: string) {
    await api.put(`/tenant/${tenantId}/channel/${channelId}/message/${messageId}/pin`)
  }

  async function fetchThread(tenantId: string, channelId: string, messageId: string) {
    const data = await api.get<{ items: Message[] }>(
      `/tenant/${tenantId}/channel/${channelId}/message/${messageId}/thread`,
    )
    threadMessages.value = data.items
  }

  function addMessageFromWs(msg: Message) {
    if (msg.thread_id) {
      if (!threadMessages.value.some((m) => m.id === msg.id)) {
        threadMessages.value.push(msg)
      }
    } else {
      if (!messages.value.some((m) => m.id === msg.id)) {
        messages.value.push(msg)
      }
    }
  }

  return {
    messages,
    threadMessages,
    loading,
    fetchMessages,
    sendMessage,
    editMessage,
    deleteMessage,
    addReaction,
    togglePin,
    fetchThread,
    addMessageFromWs,
  }
})
