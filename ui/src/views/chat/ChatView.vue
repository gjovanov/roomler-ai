<template>
  <v-container fluid class="fill-height pa-0">
    <v-row no-gutters class="fill-height">
      <!-- Message list -->
      <v-col class="d-flex flex-column fill-height">
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
        <div ref="messageListRef" class="flex-grow-1 overflow-y-auto pa-4">
          <div v-if="messageStore.loading" class="text-center pa-4">
            <v-progress-circular indeterminate />
          </div>
          <div v-else-if="messageStore.messages.length === 0" class="text-center pa-8 text-medium-emphasis">
            {{ $t('room.noMessages') }}
          </div>
          <div v-else>
            <div v-for="msg in messageStore.messages" :key="msg.id" class="mb-3">
              <message-bubble
                :message="msg"
                :editable="msg.author_id === currentUserId"
                @reply="openThread(msg)"
                @react="(emoji) => handleReact(msg.id, emoji)"
                @pin="handlePin(msg.id)"
                @edit="(content) => handleEdit(msg.id, content)"
              />
            </div>
          </div>
        </div>

        <!-- Message input -->
        <v-divider />
        <div class="pa-3">
          <message-editor
            ref="messageEditorRef"
            :placeholder="$t('message.placeholder')"
            :members="roomMembers"
            @send="sendMessage"
            @open-emoji-picker="showEmojiPicker = true; emojiTarget = 'editor'"
            @open-giphy-picker="showGiphyPicker = true"
          />
        </div>
      </v-col>

      <!-- Thread panel -->
      <v-col v-if="activeThread" cols="4" class="border-s d-flex flex-column fill-height">
        <v-toolbar density="compact" flat>
          <v-toolbar-title class="text-body-1">Thread</v-toolbar-title>
          <v-spacer />
          <v-btn icon="mdi-close" size="small" @click="activeThread = null" />
        </v-toolbar>
        <v-divider />
        <div class="flex-grow-1 overflow-y-auto pa-4">
          <div v-for="msg in messageStore.threadMessages" :key="msg.id" class="mb-2">
            <message-bubble
              :message="msg"
              :editable="msg.author_id === currentUserId"
              compact
              @react="(emoji) => handleReact(msg.id, emoji)"
              @edit="(content) => handleEdit(msg.id, content)"
            />
          </div>
        </div>
        <v-divider />
        <div class="pa-3">
          <message-editor
            ref="threadEditorRef"
            placeholder="Reply in thread..."
            :members="roomMembers"
            @send="sendThreadReply"
            @open-emoji-picker="showEmojiPicker = true; emojiTarget = 'thread'"
            @open-giphy-picker="showGiphyPicker = true"
          />
        </div>
      </v-col>

      <!-- Members panel -->
      <v-col v-if="showMembers" cols="3" class="border-s">
        <v-toolbar density="compact" flat>
          <v-toolbar-title class="text-body-1">Members</v-toolbar-title>
          <v-spacer />
          <v-btn icon="mdi-close" size="small" @click="showMembers = false" />
        </v-toolbar>
        <v-divider />
        <v-list density="compact">
          <v-list-item prepend-icon="mdi-account" title="Members list coming soon" />
        </v-list>
      </v-col>
    </v-row>

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
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, nextTick, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useRoomStore } from '@/stores/rooms'
import { useMessageStore } from '@/stores/messages'
import { useWsStore } from '@/stores/ws'
import MessageBubble from '@/components/chat/MessageBubble.vue'
import MessageEditor from '@/components/chat/MessageEditor.vue'
import type { MentionData } from '@/components/chat/MessageEditor.vue'
import type { MentionItem } from '@/components/chat/MentionList.vue'
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

const isCallActive = computed(() => roomStore.current?.conference_status === 'InProgress')

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

async function fetchRoomMembers() {
  if (!tenantId.value || !roomId.value) return
  try {
    const members = await roomStore.fetchMembers(tenantId.value, roomId.value)
    roomMembers.value = members.map((m) => ({
      id: m.id,
      user_id: m.user_id,
      display_name: m.display_name,
    }))
  } catch {
    // members list not critical
  }
}

async function sendMessage(content: string, mentions?: MentionData) {
  if (!content) return
  await messageStore.sendMessage(tenantId.value, roomId.value, content, undefined, mentions)
  await nextTick()
  scrollToBottom()
}

async function sendThreadReply(content: string, mentions?: MentionData) {
  if (!content || !activeThread.value) return
  await messageStore.sendMessage(tenantId.value, roomId.value, content, activeThread.value.id, mentions)
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

watch(roomId, async (id) => {
  if (id) {
    fetchRoomMembers()
    await messageStore.fetchMessages(tenantId.value, id)
    await nextTick()
    scrollToBottom()
  }
})

onMounted(async () => {
  if (roomId.value) {
    roomStore.fetchRoom(tenantId.value, roomId.value)
    fetchRoomMembers()
    await messageStore.fetchMessages(tenantId.value, roomId.value)
    await nextTick()
    scrollToBottom()
  }
})
</script>
