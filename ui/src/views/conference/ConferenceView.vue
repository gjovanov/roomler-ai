<template>
  <v-container fluid class="fill-height pa-0">
    <v-row no-gutters class="fill-height">
      <v-col class="d-flex flex-column fill-height">
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
          <TranscriptionSwitcher
            v-if="joined"
            :enabled="roomStore.transcriptionEnabled"
            :selected-model="roomStore.selectedTranscriptionModel"
            @update:model="roomStore.setSelectedTranscriptionModel"
            @toggle="toggleTranscription"
          />
          <v-btn
            v-if="joined && roomStore.transcriptionEnabled"
            :icon="showTranscriptPanel ? 'mdi-text-box' : 'mdi-text-box-outline'"
            variant="text"
            :color="showTranscriptPanel ? 'primary' : undefined"
            @click="showTranscriptPanel = !showTranscriptPanel"
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

            <!-- Transcript subtitle overlay -->
            <TranscriptOverlay
              v-if="joined && roomStore.transcriptionEnabled"
              :segments="roomStore.transcriptSegments"
            />
          </div>

          <!-- Transcript panel -->
          <TranscriptPanel
            v-if="joined && showTranscriptPanel && roomStore.transcriptionEnabled"
            :segments="roomStore.transcriptSegments"
          />

          <!-- Files panel -->
          <ConferenceFilesPanel
            v-if="joined && showFiles"
            :tenant-id="tenantId"
            :room-id="roomId"
            @play="handlePlayFile"
          />

          <!-- Chat panel -->
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

            <!-- Messages list -->
            <div ref="chatListRef" class="flex-grow-1 overflow-y-auto pa-3">
              <div
                v-if="roomStore.chatMessages.length === 0"
                class="text-center text-medium-emphasis mt-4"
              >
                {{ $t('call.noChatMessages') }}
              </div>
              <div
                v-for="msg in roomStore.chatMessages"
                :key="msg.id"
                class="mb-3"
              >
                <div class="d-flex align-start">
                  <v-avatar size="28" color="primary" class="mr-2 mt-1">
                    <span class="text-caption">{{ msg.display_name.charAt(0).toUpperCase() }}</span>
                  </v-avatar>
                  <div class="flex-grow-1" style="min-width: 0;">
                    <div class="d-flex align-center ga-2">
                      <span class="text-subtitle-2 font-weight-bold text-truncate">
                        {{ msg.display_name }}
                      </span>
                      <span class="text-caption text-medium-emphasis flex-shrink-0">
                        {{ formatTime(msg.created_at) }}
                      </span>
                    </div>
                    <div class="text-body-2" style="word-break: break-word;">{{ msg.content }}</div>
                  </div>
                </div>
              </div>
            </div>

            <v-divider />

            <!-- Chat input -->
            <div class="pa-2">
              <v-text-field
                v-model="chatInput"
                :placeholder="$t('call.chatPlaceholder')"
                variant="outlined"
                density="compact"
                hide-details
                append-inner-icon="mdi-send"
                @keydown.enter.exact.prevent="handleSendChat"
                @click:append-inner="handleSendChat"
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
      </v-col>
    </v-row>

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
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, watch, nextTick, type Component } from 'vue'
import { storeToRefs } from 'pinia'
import { useRoute, useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useRoomStore } from '@/stores/rooms'
import { useWsStore } from '@/stores/ws'
import { useConferenceStore } from '@/stores/conference'
import { useAudioPlayback } from '@/composables/useAudioPlayback'
import { useConferenceLayout } from '@/composables/useConferenceLayout'
import { usePictureInPicture } from '@/composables/usePictureInPicture'
import LayoutSwitcher from '@/components/conference/LayoutSwitcher.vue'
import TranscriptionSwitcher from '@/components/conference/TranscriptionSwitcher.vue'
import TranscriptOverlay from '@/components/conference/TranscriptOverlay.vue'
import TranscriptPanel from '@/components/conference/TranscriptPanel.vue'
import ConferenceFilesPanel from '@/components/conference/ConferenceFilesPanel.vue'
import TiledLayout from '@/components/conference/layouts/TiledLayout.vue'
import SpotlightLayout from '@/components/conference/layouts/SpotlightLayout.vue'
import SidebarLayout from '@/components/conference/layouts/SidebarLayout.vue'

const route = useRoute()
const router = useRouter()
const authStore = useAuthStore()
const roomStore = useRoomStore()
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

const tenantId = computed(() => route.params.tenantId as string)
const roomId = computed(() => route.params.roomId as string)
const joined = ref(false)
const joining = ref(false)
const showChat = ref(false)
const showTranscriptPanel = ref(false)
const showFiles = ref(false)
const showSwitchDialog = ref(false)
const chatInput = ref('')
const chatListRef = ref<HTMLElement | null>(null)

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
    case 'InProgress': return 'success'
    case 'Scheduled': return 'info'
    case 'Ended': return 'grey'
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
  () => roomStore.chatMessages.length,
  async () => {
    await nextTick()
    scrollChatToBottom()
  },
)

async function handleSendChat() {
  const content = chatInput.value.trim()
  if (!content) return
  chatInput.value = ''
  try {
    await roomStore.sendChatMessage(tenantId.value, roomId.value, content)
  } catch (err) {
    console.error('Failed to send chat message:', err)
  }
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
    conferenceStore.startActiveSpeaker()
    wsStore.onMediaMessage('media:audio_playback', audioPlayback.handlePlaybackMessage)
    roomStore.fetchChatMessages(tenantId.value, roomId.value).catch(() => {})
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
    roomStore.fetchChatMessages(tenantId.value, roomId.value).catch(() => {})

    joined.value = true
    showChat.value = true

    conferenceStore.startActiveSpeaker()
    wsStore.onMediaMessage('media:audio_playback', audioPlayback.handlePlaybackMessage)
  } catch (err) {
    console.error('Failed to join call:', err)
  } finally {
    joining.value = false
  }
}

async function handleLeave() {
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
  roomStore.clearChatMessages()
  roomStore.clearTranscript()
  roomStore.setTranscriptionEnabled(false)
  roomStore.clearRoomFiles()
  await roomStore.leaveCall(tenantId.value, roomId.value)
  joined.value = false
  showChat.value = false
  showTranscriptPanel.value = false
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

function toggleTranscription(newState: boolean) {
  wsStore.send('media:transcript_toggle', {
    room_id: roomId.value,
    enabled: newState,
    model: roomStore.selectedTranscriptionModel,
  })
  roomStore.setTranscriptionEnabled(newState)
  if (newState) {
    showTranscriptPanel.value = true
  }
}

function handleScreenShare() {
  if (conferenceStore.isScreenSharing) {
    conferenceStore.stopScreenShare()
  } else {
    conferenceStore.startScreenShare()
  }
}

onMounted(async () => {
  try {
    await roomStore.fetchRoom(tenantId.value, roomId.value)
  } catch {
    // Room not found
  }

  // If already in call for this room (returned from mini-view), restore full view
  if (conferenceStore.isInCall && conferenceStore.roomId === roomId.value) {
    joined.value = true
    showChat.value = true
    conferenceStore.startActiveSpeaker()
    wsStore.onMediaMessage('media:audio_playback', audioPlayback.handlePlaybackMessage)
    roomStore.fetchChatMessages(tenantId.value, roomId.value).catch(() => {})
    await roomStore.fetchParticipants(tenantId.value, roomId.value)
  }
})
</script>

<style scoped>
.chat-panel {
  width: 320px;
  min-width: 280px;
  border-left: 1px solid rgba(var(--v-border-color), var(--v-border-opacity));
  background: rgb(var(--v-theme-surface));
}
</style>
