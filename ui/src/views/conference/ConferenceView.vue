<template>
  <div class="conference-root">
    <div class="conference-row">
      <div class="conference-main-col">
        <!-- Call header -->
        <v-toolbar density="compact" flat>
          <v-toolbar-title>
            {{ roomStore.current?.name || 'Call' }}
          </v-toolbar-title>
          <v-spacer />
          <LayoutSwitcher
            v-if="joined"
            :prefs="layoutCtrl.prefs.value"
            @update:mode="layoutCtrl.setMode"
            @update:max-tiles="layoutCtrl.setMaxTiles"
            @update:hide-non-video="layoutCtrl.setHideNonVideo"
            @update:self-view-mode="layoutCtrl.setSelfViewMode"
          />
          <v-btn
            v-if="joined"
            :icon="showFiles ? 'mdi-folder' : 'mdi-folder-outline'"
            variant="text"
            :color="showFiles ? 'primary' : undefined"
            @click="toggleFiles"
          />
          <v-btn
            v-if="joined"
            :icon="showChat ? 'mdi-message-text' : 'mdi-message-text-outline'"
            variant="text"
            :color="showChat ? 'primary' : undefined"
            @click="showChat = !showChat"
          />
          <v-chip size="small" :color="statusColor">
            {{ roomStore.current?.conference_status }}
          </v-chip>
        </v-toolbar>

        <v-divider />

        <!-- Main content area -->
        <div class="flex-grow-1 d-flex overflow-hidden">
          <!-- Video grid -->
          <div class="flex-grow-1 d-flex align-center justify-center bg-black position-relative">
            <!-- Pre-join state -->
            <div v-if="!joined" class="text-center text-white">
              <v-icon size="64" class="mb-4">mdi-video</v-icon>
              <div class="text-h5">Ready to join?</div>
              <div class="mt-4" style="width: 300px; margin: 0 auto;">
                <v-select
                  v-model="conferenceStore.selectedAudioDeviceId"
                  :items="audioInputs"
                  item-title="label"
                  item-value="deviceId"
                  label="Microphone"
                  density="compact"
                  variant="outlined"
                  class="mb-2"
                  hide-details
                  theme="dark"
                />
                <v-select
                  v-model="conferenceStore.selectedVideoDeviceId"
                  :items="videoInputs"
                  item-title="label"
                  item-value="deviceId"
                  label="Camera"
                  density="compact"
                  variant="outlined"
                  class="mb-2"
                  hide-details
                  theme="dark"
                />
              </div>
              <v-btn
                color="primary"
                size="large"
                class="mt-4"
                :loading="joining"
                @click="handleJoin"
              >
                {{ $t('call.join') }}
              </v-btn>
            </div>

            <!-- Active call: dynamic layout -->
            <component
              v-else
              :is="layoutComponent"
              v-bind="layoutProps"
              @toggle-pin="handleTogglePin"
              @request-pip="handlePiP"
            />

          </div>

          <!-- Files panel -->
          <ConferenceFilesPanel
            v-if="joined && showFiles"
            :tenant-id="tenantId"
            :room-id="roomId"
            @play="handlePlayFile"
          />

          <!-- Chat panel (unified with room messages) -->
          <div
            v-if="joined && showChat"
            class="chat-panel d-flex flex-column"
          >
            <v-toolbar density="compact" flat>
              <v-toolbar-title class="text-body-1">
                {{ $t('call.chat') }}
              </v-toolbar-title>
            </v-toolbar>
            <v-divider />

            <!-- Messages list using shared messageStore -->
            <div ref="chatListRef" class="flex-grow-1 overflow-y-auto pa-3">
              <div
                v-if="messageStore.messages.length === 0"
                class="text-center text-medium-emphasis mt-4"
              >
                {{ $t('call.noChatMessages') }}
              </div>
              <div
                v-for="msg in messageStore.messages"
                :key="msg.id"
                class="mb-2"
              >
                <MessageBubble
                  :message="msg"
                  :editable="msg.author_id === currentUserId"
                  compact
                  @reply="handleReplyInChat(msg)"
                  @react="(emoji) => handleReact(msg.id, emoji)"
                  @edit="(content) => handleEdit(msg.id, content)"
                  @delete="handleDelete(msg.id)"
                />
              </div>
            </div>

            <v-divider />

            <!-- Chat input using MessageEditor -->
            <div class="pa-2">
              <MessageEditor
                ref="chatEditorRef"
                :placeholder="$t('call.chatPlaceholder')"
                :members="roomMembers"
                :tenant-id="tenantId"
                :room-id="roomId"
                @send="handleSendChat"
              />
            </div>
          </div>
        </div>

        <!-- Controls bar -->
        <v-toolbar v-if="joined" density="compact" color="grey-darken-4">
          <v-spacer />
          <v-btn
            :icon="conferenceStore.isMuted ? 'mdi-microphone-off' : 'mdi-microphone'"
            :color="conferenceStore.isMuted ? 'error' : undefined"
            @click="conferenceStore.toggleMute()"
          />
          <v-btn
            :icon="conferenceStore.isVideoOn ? 'mdi-video' : 'mdi-video-off'"
            :color="!conferenceStore.isVideoOn ? 'error' : undefined"
            @click="conferenceStore.toggleVideo()"
          />
          <v-btn
            :icon="conferenceStore.isScreenSharing ? 'mdi-monitor-off' : 'mdi-monitor-share'"
            :color="conferenceStore.isScreenSharing ? 'success' : undefined"
            @click="handleScreenShare"
          />
          <v-btn
            v-if="pip.isSupported.value"
            icon="mdi-picture-in-picture-bottom-right"
            :color="pip.isPiPActive.value ? 'primary' : undefined"
            @click="togglePiP"
          />
          <v-btn icon="mdi-phone-hangup" color="error" @click="handleLeave" />
          <v-spacer />
        </v-toolbar>
      </div>
    </div>

    <!-- Confirmation dialog for switching calls -->
    <v-dialog v-model="showSwitchDialog" max-width="400">
      <v-card>
        <v-card-title class="text-h6">Already in a call</v-card-title>
        <v-card-text>
          You are already in a call in "{{ conferenceStore.roomName }}".
          Leave that call and join this one?
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="showSwitchDialog = false">Cancel</v-btn>
          <v-btn color="primary" variant="tonal" @click="confirmSwitch">Switch Call</v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </div>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, watch, nextTick, type Component } from 'vue'
import { storeToRefs } from 'pinia'
import { useRoute, useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useRoomStore } from '@/stores/rooms'
import { useMessageStore } from '@/stores/messages'
import { useWsStore } from '@/stores/ws'
import { useConferenceStore } from '@/stores/conference'
import { useSnackbar } from '@/composables/useSnackbar'
import { useAudioPlayback } from '@/composables/useAudioPlayback'
import { useConferenceLayout } from '@/composables/useConferenceLayout'
import { usePictureInPicture } from '@/composables/usePictureInPicture'
import LayoutSwitcher from '@/components/conference/LayoutSwitcher.vue'
import ConferenceFilesPanel from '@/components/conference/ConferenceFilesPanel.vue'
import MessageBubble from '@/components/chat/MessageBubble.vue'
import MessageEditor from '@/components/chat/MessageEditor.vue'
import type { MentionData } from '@/components/chat/MessageEditor.vue'
import type { MentionItem } from '@/components/chat/MentionList.vue'
import TiledLayout from '@/components/conference/layouts/TiledLayout.vue'
import SpotlightLayout from '@/components/conference/layouts/SpotlightLayout.vue'
import SidebarLayout from '@/components/conference/layouts/SidebarLayout.vue'

const route = useRoute()
const router = useRouter()
const authStore = useAuthStore()
const roomStore = useRoomStore()
const messageStore = useMessageStore()
const wsStore = useWsStore()
const conferenceStore = useConferenceStore()
const {
  localStream,
  isMuted: isMutedRef,
  audioLevels: audioLevelsRef,
  activeSpeakerKey: activeSpeakerKeyRef,
} = storeToRefs(conferenceStore)
const pip = usePictureInPicture()
const audioPlayback = useAudioPlayback()
const snackbar = useSnackbar()

const audioInputs = computed(() =>
  conferenceStore.availableDevices
    .filter((d) => d.kind === 'audioinput')
    .map((d, i) => ({ label: d.label || 'Device ' + i, deviceId: d.deviceId }))
)
const videoInputs = computed(() =>
  conferenceStore.availableDevices
    .filter((d) => d.kind === 'videoinput')
    .map((d, i) => ({ label: d.label || 'Device ' + i, deviceId: d.deviceId }))
)

const tenantId = computed(() => route.params.tenantId as string)
const roomId = computed(() => route.params.roomId as string)
const joined = ref(false)
const joining = ref(false)
const showChat = ref(false)
const showFiles = ref(false)
const showSwitchDialog = ref(false)
const chatEditorRef = ref<InstanceType<typeof MessageEditor> | null>(null)
const chatListRef = ref<HTMLElement | null>(null)
const roomMembers = ref<MentionItem[]>([])

const currentUserId = computed(() => authStore.user?.id)

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

const localDisplayName = computed(() => authStore.user?.display_name ?? 'You')

function getDisplayName(userId: string): string {
  const participant = roomStore.participants.find((p) => p.user_id === userId)
  return participant?.display_name || userId.slice(0, 8)
}

const layoutCtrl = useConferenceLayout(
  localStream,
  conferenceStore.remoteStreams,
  isMutedRef,
  audioLevelsRef,
  activeSpeakerKeyRef,
  getDisplayName,
  localDisplayName,
)

const layoutComponent = computed<Component>(() => {
  switch (layoutCtrl.layout.value.effectiveMode) {
    case 'spotlight': return SpotlightLayout
    case 'sidebar': return SidebarLayout
    default: return TiledLayout
  }
})

const layoutProps = computed(() => {
  const l = layoutCtrl.layout.value
  const selfP = layoutCtrl.selfParticipant.value
  const speakerKey = conferenceStore.activeSpeakerKey

  if (l.effectiveMode === 'tiled') {
    return {
      participants: l.primary,
      selfParticipant: l.selfViewFloating ? selfP : null,
      selfViewFloating: l.selfViewFloating,
      selfViewMode: layoutCtrl.prefs.value.selfViewMode,
      activeSpeakerKey: speakerKey,
    }
  }
  // spotlight or sidebar
  return {
    primary: l.primary,
    secondary: l.secondary,
    selfParticipant: l.selfViewFloating ? selfP : null,
    selfViewFloating: l.selfViewFloating,
    activeSpeakerKey: speakerKey,
  }
})

function handleTogglePin(streamKey: string) {
  layoutCtrl.togglePin(streamKey)
}

function handlePiP(streamKey: string) {
  const videoEl = document.querySelector(
    `video[data-stream-key="${streamKey}"]`,
  ) as HTMLVideoElement | null
  if (videoEl) {
    pip.requestPiP(videoEl)
  }
}

function togglePiP() {
  if (pip.isPiPActive.value) {
    pip.exitPiP()
  } else {
    const speakerKey = conferenceStore.activeSpeakerKey
    const targetKey = speakerKey || (conferenceStore.remoteStreams.keys().next().value as string | undefined)
    if (targetKey) {
      handlePiP(targetKey)
    }
  }
}

const statusColor = computed(() => {
  switch (roomStore.current?.conference_status) {
    case 'in_progress': return 'success'
    case 'scheduled': return 'info'
    case 'ended': return 'grey'
    default: return 'warning'
  }
})

function formatTime(iso: string): string {
  const d = new Date(iso)
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
}

function scrollChatToBottom() {
  if (chatListRef.value) {
    chatListRef.value.scrollTop = chatListRef.value.scrollHeight
  }
}

// Auto-scroll when new messages arrive
watch(
  () => messageStore.messages.length,
  async () => {
    await nextTick()
    scrollChatToBottom()
  },
)

async function handleSendChat(content: string, mentions?: MentionData, attachmentIds?: string[]) {
  if (!content && (!attachmentIds || attachmentIds.length === 0)) return
  try {
    await messageStore.sendMessage(tenantId.value, roomId.value, content, undefined, mentions, attachmentIds)
  } catch (err) {
    console.error('Failed to send chat message:', err)
  }
}

function handleReplyInChat(msg: { id: string; author_name: string; content: string }) {
  // Quote the message content as a reply
  const quote = msg.content.split('\n').map((l) => `> ${l}`).join('\n')
  const replyPrefix = `> **${msg.author_name}**:\n${quote}\n\n`
  chatEditorRef.value?.setContent(replyPrefix)
  chatEditorRef.value?.focus()
}

async function handleReact(messageId: string, emoji: string) {
  await messageStore.toggleReaction(tenantId.value, roomId.value, messageId, emoji)
}

async function handleEdit(messageId: string, content: string) {
  await messageStore.editMessage(tenantId.value, roomId.value, messageId, content)
}

async function handleDelete(messageId: string) {
  await messageStore.deleteMessage(tenantId.value, roomId.value, messageId)
}

async function confirmSwitch() {
  showSwitchDialog.value = false
  // Leave the current call first
  if (conferenceStore.tenantId && conferenceStore.roomId) {
    await roomStore.leaveCall(conferenceStore.tenantId, conferenceStore.roomId)
  }
  conferenceStore.leaveRoom()
  // Now join the new call
  await doJoin()
}

async function handleJoin() {
  if (joined.value || joining.value) return

  // If already in a different call, show confirmation dialog
  if (conferenceStore.isInCall && conferenceStore.roomId !== roomId.value) {
    showSwitchDialog.value = true
    return
  }

  // If returning to the same call, just show the full UI
  if (conferenceStore.isInCall && conferenceStore.roomId === roomId.value) {
    joined.value = true
    showChat.value = true
    window.addEventListener('beforeunload', warnBeforeLeave)
    conferenceStore.startActiveSpeaker()
    wsStore.onMediaMessage('media:audio_playback', audioPlayback.handlePlaybackMessage)
    messageStore.fetchMessages(tenantId.value, roomId.value).catch(() => {})
    await roomStore.fetchParticipants(tenantId.value, roomId.value)
    return
  }

  await doJoin()
}

async function doJoin() {
  joining.value = true
  try {
    await roomStore.startCall(tenantId.value, roomId.value)
    await roomStore.joinCall(tenantId.value, roomId.value)
    await roomStore.fetchRoom(tenantId.value, roomId.value)

    // Use conference store instead of composable
    const rName = roomStore.current?.name || 'Call'
    await conferenceStore.joinRoom(tenantId.value, roomId.value, rName)
    await conferenceStore.produceLocalMedia()
    await roomStore.fetchParticipants(tenantId.value, roomId.value)
    fetchRoomMembers()
    messageStore.fetchMessages(tenantId.value, roomId.value).catch(() => {})

    joined.value = true
    showChat.value = true
    window.addEventListener('beforeunload', warnBeforeLeave)

    conferenceStore.startActiveSpeaker()
    wsStore.onMediaMessage('media:audio_playback', audioPlayback.handlePlaybackMessage)
  } catch (err: unknown) {
    const error = err as Error
    if (error.name === 'NotAllowedError') {
      snackbar.showError('Camera/microphone access denied. Please allow permissions in your browser settings.')
    } else if (error.name === 'NotFoundError') {
      snackbar.showError('No camera or microphone found. Please connect a device and try again.')
    } else {
      snackbar.showError('Failed to join call: ' + error.message)
    }
  } finally {
    joining.value = false
  }
}

async function handleLeave() {
  // Remove beforeunload warning
  window.removeEventListener('beforeunload', warnBeforeLeave)

  // Stop active speaker (store-managed)
  conferenceStore.stopActiveSpeaker()

  // Exit PiP if active
  if (pip.isPiPActive.value) {
    await pip.exitPiP()
  }

  // Clean up audio playback
  audioPlayback.stopCurrentPlayback()
  wsStore.offMediaMessage('media:audio_playback')

  // Leave via conference store (tears down mediasoup)
  conferenceStore.leaveRoom()
  roomStore.clearRoomFiles()
  await roomStore.leaveCall(tenantId.value, roomId.value)
  joined.value = false
  showChat.value = false
  showFiles.value = false
  router.push(`/tenant/${tenantId.value}`)
}

function toggleFiles() {
  showFiles.value = !showFiles.value
  if (showFiles.value) {
    roomStore.fetchRoomFiles(tenantId.value, roomId.value)
  }
}

function handlePlayFile(file: { id: string; url: string; filename: string }) {
  wsStore.send('media:play_audio', {
    room_id: roomId.value,
    file_id: file.id,
  })
}

async function handleScreenShare() {
  if (conferenceStore.isScreenSharing) {
    conferenceStore.stopScreenShare()
  } else {
    try {
      await conferenceStore.startScreenShare()
    } catch (err: unknown) {
      const error = err as Error
      if (error.name === 'AbortError' || error.name === 'NotAllowedError') {
        // User cancelled the screen share picker — do nothing
      } else {
        snackbar.showError('Screen share failed: ' + error.message)
      }
    }
  }
}

const warnBeforeLeave = (e: BeforeUnloadEvent) => {
  e.preventDefault()
}

onMounted(async () => {
  // Enumerate devices for pre-join selection
  conferenceStore.enumerateDevices()

  try {
    await roomStore.fetchRoom(tenantId.value, roomId.value)
  } catch {
    // Room not found
  }

  // If already in call for this room (returned from mini-view), restore full view
  if (conferenceStore.isInCall && conferenceStore.roomId === roomId.value) {
    joined.value = true
    showChat.value = true
    window.addEventListener('beforeunload', warnBeforeLeave)
    conferenceStore.startActiveSpeaker()
    wsStore.onMediaMessage('media:audio_playback', audioPlayback.handlePlaybackMessage)
    fetchRoomMembers()
    messageStore.fetchMessages(tenantId.value, roomId.value).catch(() => {})
    await roomStore.fetchParticipants(tenantId.value, roomId.value)
  }
})

onUnmounted(() => {
  window.removeEventListener('beforeunload', warnBeforeLeave)
})
</script>

<style scoped>
.conference-root {
  display: flex;
  flex-direction: column;
  flex: 1 1 0;
  min-height: 0;
  overflow: hidden;
}
.conference-row {
  display: flex;
  flex: 1 1 0;
  min-height: 0;
  overflow: hidden;
}
.conference-main-col {
  display: flex;
  flex-direction: column;
  flex: 1 1 0;
  min-height: 0;
  min-width: 0;
  overflow: hidden;
}
.chat-panel {
  width: 320px;
  min-width: 280px;
  min-height: 0;
  border-left: 1px solid rgba(var(--v-border-color), var(--v-border-opacity));
  background: rgb(var(--v-theme-surface));
}
</style>
