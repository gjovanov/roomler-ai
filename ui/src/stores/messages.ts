import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

interface Reaction {
  emoji: string
  count: number
}

export interface MentionData {
  users: string[]
  everyone: boolean
  here: boolean
}

interface Message {
  id: string
  room_id: string
  author_id: string
  author_name?: string
  content: string
  thread_id?: string
  is_thread_root: boolean
  is_pinned: boolean
  reaction_summary: Reaction[]
  mentions?: MentionData
  created_at: string
  updated_at: string
}

export const useMessageStore = defineStore('messages', () => {
  const messages = ref<Message[]>([])
  const threadMessages = ref<Message[]>([])
  const loading = ref(false)
  // Track which emojis the current user has reacted with per message: { message_id: Set<emoji> }
  const userReactions = ref<Record<string, Set<string>>>({})

  async function fetchMessages(tenantId: string, roomId: string) {
    loading.value = true
    try {
      const data = await api.get<{ items: Message[] }>(
        `/tenant/${tenantId}/room/${roomId}/message`,
      )
      messages.value = data.items
    } finally {
      loading.value = false
    }
  }

  async function sendMessage(tenantId: string, roomId: string, content: string, threadId?: string, mentions?: MentionData) {
    const body: Record<string, unknown> = { content, thread_id: threadId }
    if (mentions && (mentions.users.length > 0 || mentions.everyone || mentions.here)) {
      body.mentions = mentions
    }
    const msg = await api.post<Message>(
      `/tenant/${tenantId}/room/${roomId}/message`,
      body,
    )
    if (threadId) {
      threadMessages.value.push(msg)
    } else {
      messages.value.push(msg)
    }
    return msg
  }

  async function editMessage(tenantId: string, roomId: string, messageId: string, content: string) {
    const msg = await api.put<Message>(
      `/tenant/${tenantId}/room/${roomId}/message/${messageId}`,
      { content },
    )
    const idx = messages.value.findIndex((m) => m.id === messageId)
    if (idx !== -1) messages.value[idx] = msg
    return msg
  }

  async function deleteMessage(tenantId: string, roomId: string, messageId: string) {
    await api.delete(`/tenant/${tenantId}/room/${roomId}/message/${messageId}`)
    messages.value = messages.value.filter((m) => m.id !== messageId)
  }

  async function addReaction(tenantId: string, roomId: string, messageId: string, emoji: string) {
    await api.post(`/tenant/${tenantId}/room/${roomId}/message/${messageId}/reaction`, {
      emoji,
    })
    // Track locally
    if (!userReactions.value[messageId]) {
      userReactions.value[messageId] = new Set()
    }
    userReactions.value[messageId].add(emoji)
  }

  async function removeReaction(tenantId: string, roomId: string, messageId: string, emoji: string) {
    await api.delete(`/tenant/${tenantId}/room/${roomId}/message/${messageId}/reaction/${encodeURIComponent(emoji)}`)
    userReactions.value[messageId]?.delete(emoji)
  }

  function hasUserReacted(messageId: string, emoji: string): boolean {
    return userReactions.value[messageId]?.has(emoji) ?? false
  }

  async function toggleReaction(tenantId: string, roomId: string, messageId: string, emoji: string) {
    if (hasUserReacted(messageId, emoji)) {
      await removeReaction(tenantId, roomId, messageId, emoji)
    } else {
      await addReaction(tenantId, roomId, messageId, emoji)
    }
  }

  function handleReactionFromWs(data: { action: string; message_id: string; emoji: string; user_id: string }) {
    const updateList = (list: Message[]) => {
      const msg = list.find((m) => m.id === data.message_id)
      if (!msg) return
      if (data.action === 'add') {
        const existing = msg.reaction_summary.find((r) => r.emoji === data.emoji)
        if (existing) {
          existing.count++
        } else {
          msg.reaction_summary.push({ emoji: data.emoji, count: 1 })
        }
      } else if (data.action === 'remove') {
        const existing = msg.reaction_summary.find((r) => r.emoji === data.emoji)
        if (existing) {
          existing.count--
          if (existing.count <= 0) {
            msg.reaction_summary = msg.reaction_summary.filter((r) => r.emoji !== data.emoji)
          }
        }
      }
    }
    updateList(messages.value)
    updateList(threadMessages.value)
  }

  async function togglePin(tenantId: string, roomId: string, messageId: string) {
    await api.put(`/tenant/${tenantId}/room/${roomId}/message/${messageId}/pin`)
  }

  async function fetchThread(tenantId: string, roomId: string, messageId: string) {
    const data = await api.get<{ items: Message[] }>(
      `/tenant/${tenantId}/room/${roomId}/message/${messageId}/thread`,
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
    userReactions,
    addReaction,
    removeReaction,
    toggleReaction,
    hasUserReacted,
    handleReactionFromWs,
    togglePin,
    fetchThread,
    addMessageFromWs,
  }
})
