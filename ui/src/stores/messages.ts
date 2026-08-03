import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'
import { useAuthStore } from './auth'

interface Reaction {
  emoji: string
  count: number
}

export interface MentionData {
  users: string[]
  everyone: boolean
  here: boolean
}

interface Attachment {
  file_id: string
  filename: string
  content_type: string
  size: number
  url: string
  thumbnail_url?: string
}

interface Message {
  id: string
  room_id: string
  author_id: string
  author_name: string
  content: string
  thread_id?: string
  is_thread_root: boolean
  is_pinned: boolean
  is_edited: boolean
  reaction_summary: Reaction[]
  attachments: Attachment[]
  mentions?: MentionData
  is_read?: boolean
  reply_count?: number
  last_reply_at?: string
  last_reply_user_id?: string
  created_at: string
  updated_at: string
}

export const useMessageStore = defineStore('messages', () => {
  const messages = ref<Message[]>([])
  const threadMessages = ref<Message[]>([])
  const loading = ref(false)
  const loadingMore = ref(false)
  const hasMore = ref(true)
  const PAGE_SIZE = 25
  // Track which emojis the current user has reacted with per message: { message_id: Set<emoji> }
  const userReactions = ref<Record<string, Set<string>>>({})

  // Room whose messages currently fill `messages` — WS-pushed messages for
  // OTHER rooms must not leak into the open list (they used to be appended
  // to whatever room was on screen).
  const currentRoomId = ref<string | null>(null)

  async function fetchMessages(tenantId: string, roomId: string) {
    loading.value = true
    hasMore.value = true
    currentRoomId.value = roomId
    try {
      const data = await api.get<{ items: Message[]; total: number }>(
        `/tenant/${tenantId}/room/${roomId}/message?per_page=${PAGE_SIZE}`,
      )
      // The wire is newest-first (DESC, locked by pagination tests); the UI
      // renders top→bottom ascending and anchors at the bottom, so reverse
      // each page on ingest. (2026-08-04: rendering the DESC page raw put
      // the newest messages at the TOP while the view scrolled to the
      // bottom — "my messages are gone after refresh".)
      messages.value = [...data.items].reverse()
      hasMore.value = data.items.length < data.total
    } finally {
      loading.value = false
    }
  }

  async function fetchOlderMessages(tenantId: string, roomId: string) {
    if (loadingMore.value || !hasMore.value || messages.value.length === 0) return
    loadingMore.value = true
    try {
      // Ascending store ⇒ [0] IS the oldest. (Under the old DESC store this
      // grabbed the NEWEST message, so `before` re-fetched the same page
      // forever and genuinely older messages were unreachable.)
      const oldest = messages.value[0]
      const data = await api.get<{ items: Message[]; total: number }>(
        `/tenant/${tenantId}/room/${roomId}/message?per_page=${PAGE_SIZE}&before=${oldest.created_at}`,
      )
      if (data.items.length === 0) {
        hasMore.value = false
      } else {
        const known = new Set(messages.value.map((m) => m.id))
        const older = [...data.items].reverse().filter((m) => !known.has(m.id))
        messages.value = [...older, ...messages.value]
        if (data.items.length < PAGE_SIZE) hasMore.value = false
      }
    } finally {
      loadingMore.value = false
    }
  }

  async function sendMessage(tenantId: string, roomId: string, content: string, threadId?: string, mentions?: MentionData, attachmentIds?: string[]) {
    const body: Record<string, unknown> = { content, thread_id: threadId }
    if (mentions && (mentions.users.length > 0 || mentions.everyone || mentions.here)) {
      body.mentions = mentions
    }
    if (attachmentIds && attachmentIds.length > 0) {
      body.attachment_ids = attachmentIds
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
    // Optimistically update reaction_summary on the message
    const updateSummary = (list: Message[]) => {
      const msg = list.find((m) => m.id === messageId)
      if (!msg) return
      const existing = msg.reaction_summary.find((r) => r.emoji === emoji)
      if (existing) {
        existing.count++
      } else {
        msg.reaction_summary.push({ emoji, count: 1 })
      }
    }
    updateSummary(messages.value)
    updateSummary(threadMessages.value)

    // Track locally
    if (!userReactions.value[messageId]) {
      userReactions.value[messageId] = new Set()
    }
    userReactions.value[messageId].add(emoji)

    await api.post(`/tenant/${tenantId}/room/${roomId}/message/${messageId}/reaction`, {
      emoji,
    })
  }

  async function removeReaction(tenantId: string, roomId: string, messageId: string, emoji: string) {
    // Optimistically update reaction_summary
    const updateSummary = (list: Message[]) => {
      const msg = list.find((m) => m.id === messageId)
      if (!msg) return
      const existing = msg.reaction_summary.find((r) => r.emoji === emoji)
      if (existing) {
        existing.count--
        if (existing.count <= 0) {
          msg.reaction_summary = msg.reaction_summary.filter((r) => r.emoji !== emoji)
        }
      }
    }
    updateSummary(messages.value)
    updateSummary(threadMessages.value)

    userReactions.value[messageId]?.delete(emoji)

    await api.delete(`/tenant/${tenantId}/room/${roomId}/message/${messageId}/reaction/${encodeURIComponent(emoji)}`)
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
    // Skip if this reaction was from the current user — already optimistically updated
    const authStore = useAuthStore()
    if (data.user_id === authStore.user?.id) return

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
    // Prepend the parent message so the thread panel shows the original message first
    const parent = messages.value.find((m) => m.id === messageId)
    if (parent && !data.items.some((m) => m.id === messageId)) {
      threadMessages.value = [parent, ...data.items]
    } else {
      threadMessages.value = data.items
    }
  }

  function addMessageFromWs(msg: Message) {
    // Only the open room's list should grow — a message for another room is
    // handled via the unread badge, not by leaking into this view.
    if (currentRoomId.value && msg.room_id !== currentRoomId.value) return
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

  function updateMessageFromWs(msg: Message) {
    const idx = messages.value.findIndex((m) => m.id === msg.id)
    if (idx !== -1) messages.value[idx] = msg
    const tidx = threadMessages.value.findIndex((m) => m.id === msg.id)
    if (tidx !== -1) threadMessages.value[tidx] = msg
  }

  function removeMessageFromWs(data: { id: string; room_id: string }) {
    messages.value = messages.value.filter((m) => m.id !== data.id)
    threadMessages.value = threadMessages.value.filter((m) => m.id !== data.id)
  }

  return {
    messages,
    threadMessages,
    currentRoomId,
    loading,
    loadingMore,
    hasMore,
    fetchMessages,
    fetchOlderMessages,
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
    updateMessageFromWs,
    removeMessageFromWs,
  }
})
