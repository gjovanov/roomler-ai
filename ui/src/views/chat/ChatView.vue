<template>
  <div class="chat-root">
    <div class="chat-row">
      <!-- Message list -->
      <div class="chat-main-col">
        <!-- Room header -->
        <v-toolbar density="compact" flat>
          <v-toolbar-title>
            <v-icon class="mr-1" size="small">mdi-pound</v-icon>
            {{ roomStore.current?.name }}
          </v-toolbar-title>
          <v-spacer />
          <v-btn
            v-if="roomStore.current?.has_media"
            :color="isCallActive ? 'success' : undefined"
            :prepend-icon="isCallActive ? 'mdi-phone' : 'mdi-video-plus'"
            size="small"
            variant="tonal"
            class="mr-2"
            @click="goToCall"
          >
            {{ isCallActive ? 'Join Call' : 'Start Call' }}
            <v-badge
              v-if="isCallActive && (roomStore.current?.participant_count ?? 0) > 0"
              :content="roomStore.current?.participant_count"
              color="success"
              inline
            />
          </v-btn>
          <v-btn icon="mdi-pin" size="small" @click="showPinned = !showPinned" />
          <v-btn icon="mdi-account-group" size="small" @click="showMembers = !showMembers" />
        </v-toolbar>

        <v-divider />

        <!-- Messages -->
        <div ref="messageListRef" class="flex-grow-1 overflow-y-auto pa-4" style="min-height: 0; position: relative;" @scroll="handleScroll">
          <div v-if="messageStore.loadingMore" class="text-center pa-2">
            <v-progress-circular size="20" indeterminate />
          </div>
          <div v-if="messageStore.loading" class="text-center pa-4">
            <v-progress-circular indeterminate />
          </div>
          <div v-else-if="messageStore.messages.length === 0" class="text-center pa-8 text-medium-emphasis">
            {{ $t('room.noMessages') }}
          </div>
          <div v-else>
            <div v-for="msg in messageStore.messages" :key="msg.id" :id="`msg-${msg.id}`" class="mb-3">
              <message-bubble
                :message="msg"
                :editable="msg.author_id === currentUserId"
                :unread="!readMessageIds.has(msg.id) && msg.author_id !== currentUserId"
                @reply="openThread(msg)"
                @react="(emoji) => handleReact(msg.id, emoji)"
                @pin="handlePin(msg.id)"
                @edit="(content) => handleEdit(msg.id, content)"
                @delete="handleDelete(msg.id)"
              />
            </div>
          </div>
          <!-- Scroll to bottom button -->
          <v-btn
            v-if="!isNearBottom"
            icon
            size="small"
            color="primary"
            style="position: sticky; bottom: 8px; float: right; z-index: 1;"
            @click="scrollToBottom(); newMessageCount = 0"
          >
            <v-icon>mdi-chevron-double-down</v-icon>
            <v-badge
              v-if="newMessageCount > 0"
              :content="newMessageCount"
              color="error"
              floating
            />
          </v-btn>
        </div>

        <!-- Message input -->
        <v-divider />
        <div class="pa-3">
          <message-editor
            ref="messageEditorRef"
            :placeholder="$t('message.placeholder')"
            :members="roomMembers"
            :tenant-id="tenantId"
            :room-id="roomId"
            @send="sendMessage"
            @open-emoji-picker="showEmojiPicker = true; emojiTarget = 'editor'"
            @open-giphy-picker="showGiphyPicker = true"
          />
        </div>
      </div>

      <!-- Thread panel -->
      <div v-if="activeThread" class="chat-side-panel border-s d-flex flex-column">
        <v-toolbar density="compact" flat>
          <v-toolbar-title class="text-body-1">Thread</v-toolbar-title>
          <v-spacer />
          <v-btn icon="mdi-close" size="small" @click="activeThread = null" />
        </v-toolbar>
        <v-divider />
        <div class="flex-grow-1 overflow-y-auto pa-4" style="min-height: 0;">
          <div v-for="msg in messageStore.threadMessages" :key="msg.id" class="mb-2">
            <message-bubble
              :message="msg"
              :editable="msg.author_id === currentUserId"
              compact
              @react="(emoji) => handleReact(msg.id, emoji)"
              @edit="(content) => handleEdit(msg.id, content)"
              @delete="handleDelete(msg.id)"
            />
          </div>
        </div>
        <v-divider />
        <div class="pa-3">
          <message-editor
            ref="threadEditorRef"
            placeholder="Reply in thread..."
            :members="roomMembers"
            :tenant-id="tenantId"
            :room-id="roomId"
            @send="sendThreadReply"
            @open-emoji-picker="showEmojiPicker = true; emojiTarget = 'thread'"
            @open-giphy-picker="showGiphyPicker = true"
          />
        </div>
      </div>

      <!-- Members panel -->
      <div v-if="showMembers" class="chat-side-panel border-s d-flex flex-column" style="width: 25%;">
        <v-toolbar density="compact" flat>
          <v-toolbar-title class="text-body-1">Members</v-toolbar-title>
          <v-spacer />
          <v-btn icon="mdi-close" size="small" @click="showMembers = false" />
        </v-toolbar>
        <v-divider />
        <member-panel :tenant-id="tenantId" :room-id="roomId" />
      </div>
    </div>

    <!-- Emoji picker (shared) -->
    <emoji-picker
      v-model="showEmojiPicker"
      @select="handleEmojiSelect"
    />

    <!-- Giphy picker -->
    <giphy-picker
      v-model="showGiphyPicker"
      @select="handleGiphySelect"
    />
  </div>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onBeforeUnmount, nextTick, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useRoomStore } from '@/stores/rooms'
import { useMessageStore } from '@/stores/messages'
import { useWsStore } from '@/stores/ws'
import MessageBubble from '@/components/chat/MessageBubble.vue'
import MessageEditor from '@/components/chat/MessageEditor.vue'
import type { MentionData } from '@/components/chat/MessageEditor.vue'
import type { MentionItem } from '@/components/chat/MentionList.vue'
import MemberPanel from '@/components/chat/MemberPanel.vue'
import EmojiPicker from '@/components/chat/EmojiPicker.vue'
import GiphyPicker from '@/components/chat/GiphyPicker.vue'

const route = useRoute()
const router = useRouter()
const authStore = useAuthStore()
const roomStore = useRoomStore()
const messageStore = useMessageStore()
const wsStore = useWsStore()

const currentUserId = computed(() => authStore.user?.id)

const tenantId = computed(() => route.params.tenantId as string)
const roomId = computed(() => route.params.roomId as string)

const isCallActive = computed(() => roomStore.current?.conference_status === 'in_progress')

function goToCall() {
  router.push({ name: 'room-call', params: { tenantId: tenantId.value, roomId: roomId.value } })
}

const activeThread = ref<{ id: string } | null>(null)
const showPinned = ref(false)
const showMembers = ref(false)
const showEmojiPicker = ref(false)
const showGiphyPicker = ref(false)
const emojiTarget = ref<'editor' | 'thread'>('editor')
const messageListRef = ref<HTMLElement | null>(null)
const messageEditorRef = ref<InstanceType<typeof MessageEditor> | null>(null)
const threadEditorRef = ref<InstanceType<typeof MessageEditor> | null>(null)
const roomMembers = ref<MentionItem[]>([])
const isNearBottom = ref(true)
const newMessageCount = ref(0)

function checkIfNearBottom() {
  if (!messageListRef.value) return
  const { scrollTop, scrollHeight, clientHeight } = messageListRef.value
  isNearBottom.value = scrollHeight - scrollTop - clientHeight < 150
}

async function fetchRoomMembers() {
  if (!tenantId.value || !roomId.value) return
  try {
    const members = await roomStore.fetchMembers(tenantId.value, roomId.value)
    roomMembers.value = members.map((m) => ({
      id: m.id,
      user_id: m.user_id,
      display_name: m.display_name,
      username: m.username,
    }))
  } catch {
    // members list not critical
  }
}

async function sendMessage(content: string, mentions?: MentionData, attachmentIds?: string[]) {
  if (!content && (!attachmentIds || attachmentIds.length === 0)) return
  await messageStore.sendMessage(tenantId.value, roomId.value, content, undefined, mentions, attachmentIds)
  await nextTick()
  scrollToBottom()
}

async function sendThreadReply(content: string, mentions?: MentionData, attachmentIds?: string[]) {
  if ((!content && (!attachmentIds || attachmentIds.length === 0)) || !activeThread.value) return
  await messageStore.sendMessage(tenantId.value, roomId.value, content, activeThread.value.id, mentions, attachmentIds)
}

function openThread(msg: { id: string }) {
  activeThread.value = msg
  messageStore.fetchThread(tenantId.value, roomId.value, msg.id)
}

async function handleReact(messageId: string, emoji: string) {
  await messageStore.toggleReaction(tenantId.value, roomId.value, messageId, emoji)
}

async function handlePin(messageId: string) {
  await messageStore.togglePin(tenantId.value, roomId.value, messageId)
}

async function handleEdit(messageId: string, content: string) {
  await messageStore.editMessage(tenantId.value, roomId.value, messageId, content)
}

async function handleDelete(messageId: string) {
  await messageStore.deleteMessage(tenantId.value, roomId.value, messageId)
}

function handleEmojiSelect(emoji: string) {
  const target = emojiTarget.value === 'thread' ? threadEditorRef.value : messageEditorRef.value
  target?.insertContent(emoji)
  showEmojiPicker.value = false
}

function handleGiphySelect(url: string) {
  const editorRef = activeThread.value ? threadEditorRef.value : messageEditorRef.value
  editorRef?.insertContent(`![GIF](${url})`)
  showGiphyPicker.value = false
}

function scrollToBottom() {
  if (messageListRef.value) {
    messageListRef.value.scrollTop = messageListRef.value.scrollHeight
  }
}

async function handleScroll() {
  if (!messageListRef.value) return
  checkIfNearBottom()
  if (isNearBottom.value) {
    newMessageCount.value = 0
  }
  // Load older messages when scrolled to top
  if (messageListRef.value.scrollTop === 0 && messageStore.hasMore && !messageStore.loadingMore) {
    const prevHeight = messageListRef.value.scrollHeight
    await messageStore.fetchOlderMessages(tenantId.value, roomId.value)
    await nextTick()
    // Preserve scroll position after prepending
    if (messageListRef.value) {
      messageListRef.value.scrollTop = messageListRef.value.scrollHeight - prevHeight
    }
  }
}

function scrollToMessage(messageId: string) {
  nextTick(() => {
    const el = document.getElementById(`msg-${messageId}`)
    if (el) {
      el.scrollIntoView({ behavior: 'smooth', block: 'center' })
      el.classList.add('message-highlight')
      setTimeout(() => el.classList.remove('message-highlight'), 3000)
    }
  })
}

// Auto-scroll when new messages arrive (only if near bottom)
watch(
  () => messageStore.messages.length,
  async (newLen, oldLen) => {
    if (isNearBottom.value) {
      await nextTick()
      scrollToBottom()
    } else if (newLen > oldLen) {
      newMessageCount.value += newLen - oldLen
    }
  },
)

watch(roomId, async (id) => {
  if (id) {
    readMessageIds.value.clear()
    pendingReadIds.value.clear()
    fetchRoomMembers()
    await messageStore.fetchMessages(tenantId.value, id)
    await nextTick()
    scrollToBottom()
    setupReadObserver()
  }
})

// Re-observe when new messages are added
watch(() => messageStore.messages.length, () => {
  nextTick(() => observeAllMessages())
})

onBeforeUnmount(() => {
  if (observer) {
    observer.disconnect()
    observer = null
  }
  if (readDebounceTimer) {
    clearTimeout(readDebounceTimer)
    readDebounceTimer = null
  }
})

// --- IntersectionObserver-based auto-read on scroll ---
const pendingReadIds = ref<Set<string>>(new Set())
const readMessageIds = ref<Set<string>>(new Set())
let readDebounceTimer: ReturnType<typeof setTimeout> | null = null
let observer: IntersectionObserver | null = null

function setupReadObserver() {
  if (observer) observer.disconnect()
  observer = new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (entry.isIntersecting) {
        const msgId = (entry.target as HTMLElement).id.replace('msg-', '')
        const msg = messageStore.messages.find(m => m.id === msgId)
        if (msg && msg.author_id !== currentUserId.value && !readMessageIds.value.has(msgId)) {
          pendingReadIds.value.add(msgId)
        }
      }
    }
    flushPendingReads()
  }, { root: messageListRef.value, threshold: 0.5 })

  // Observe all message elements
  observeAllMessages()
}

function observeAllMessages() {
  if (!observer || !messageListRef.value) return
  const els = messageListRef.value.querySelectorAll('[id^="msg-"]')
  els.forEach(el => observer!.observe(el))
}

function flushPendingReads() {
  if (readDebounceTimer) clearTimeout(readDebounceTimer)
  readDebounceTimer = setTimeout(async () => {
    const ids = Array.from(pendingReadIds.value)
    if (ids.length === 0) return
    pendingReadIds.value.clear()
    await roomStore.markMessagesRead(tenantId.value, roomId.value, ids)
    // Add to readMessageIds set for visual tracking
    for (const id of ids) {
      readMessageIds.value.add(id)
    }
    // Refresh actual count from server instead of optimistic decrement
    await roomStore.fetchUnreadCount(tenantId.value, roomId.value)
  }, 300)
}

onMounted(async () => {
  if (roomId.value) {
    roomStore.fetchRoom(tenantId.value, roomId.value)
    fetchRoomMembers()
    await messageStore.fetchMessages(tenantId.value, roomId.value)
    await nextTick()
    const targetMsg = route.query.msg as string | undefined
    if (targetMsg) {
      scrollToMessage(targetMsg)
    } else {
      scrollToBottom()
    }
    // Setup IntersectionObserver to auto-mark messages as read on scroll
    setupReadObserver()
  }
})
</script>

<style scoped>
.chat-root {
  display: flex;
  flex-direction: column;
  flex: 1 1 0;
  min-height: 0;
  overflow: hidden;
}
.chat-row {
  display: flex;
  flex: 1 1 0;
  min-height: 0;
  overflow: hidden;
}
.chat-main-col {
  display: flex;
  flex-direction: column;
  flex: 1 1 0;
  min-height: 0;
  min-width: 0;
  overflow: hidden;
}
.chat-side-panel {
  width: 33%;
  min-width: 280px;
  min-height: 0;
  overflow: hidden;
}
.message-highlight {
  animation: highlight-fade 3s ease-out;
}
@keyframes highlight-fade {
  0% { background: rgba(var(--v-theme-primary), 0.25); }
  100% { background: transparent; }
}
</style>
