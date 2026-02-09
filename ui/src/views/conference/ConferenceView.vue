<template>
  <v-container fluid class="fill-height pa-0">
    <v-row no-gutters class="fill-height">
      <v-col class="d-flex flex-column fill-height">
        <!-- Conference header -->
        <v-toolbar density="compact" flat>
          <v-toolbar-title>
            {{ conferenceStore.current?.subject || 'Meeting' }}
          </v-toolbar-title>
          <v-spacer />
          <v-chip size="small" :color="statusColor">
            {{ conferenceStore.current?.status }}
          </v-chip>
        </v-toolbar>

        <v-divider />

        <!-- Video grid -->
        <div class="flex-grow-1 d-flex align-center justify-center bg-black">
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
              {{ $t('conference.join') }}
            </v-btn>
          </div>

          <!-- Active conference: video grid -->
          <v-row v-else class="pa-4 video-grid" justify="center" align="center">
            <!-- Local video -->
            <v-col cols="12" sm="6" md="4" lg="3">
              <VideoTile
                :stream="mediasoup.localStream.value"
                display-name="You"
                :is-muted="mediasoup.isMuted.value"
                :is-local="true"
              />
            </v-col>

            <!-- Remote participants -->
            <v-col
              v-for="[userId, remote] in mediasoup.remoteStreams"
              :key="userId"
              cols="12" sm="6" md="4" lg="3"
            >
              <VideoTile
                :stream="remote.stream"
                :display-name="getDisplayName(userId)"
                :is-muted="false"
                :is-local="false"
              />
            </v-col>
          </v-row>
        </div>

        <!-- Controls bar -->
        <v-toolbar v-if="joined" density="compact" color="grey-darken-4">
          <v-spacer />
          <v-btn
            :icon="mediasoup.isMuted.value ? 'mdi-microphone-off' : 'mdi-microphone'"
            :color="mediasoup.isMuted.value ? 'error' : undefined"
            @click="mediasoup.toggleMute()"
          />
          <v-btn
            :icon="mediasoup.isVideoOn.value ? 'mdi-video' : 'mdi-video-off'"
            :color="!mediasoup.isVideoOn.value ? 'error' : undefined"
            @click="mediasoup.toggleVideo()"
          />
          <v-btn
            :icon="mediasoup.isScreenSharing.value ? 'mdi-monitor-off' : 'mdi-monitor-share'"
            :color="mediasoup.isScreenSharing.value ? 'success' : undefined"
            @click="handleScreenShare"
          />
          <v-btn icon="mdi-phone-hangup" color="error" @click="handleLeave" />
          <v-spacer />
        </v-toolbar>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useConferenceStore } from '@/stores/conference'
import { useMediasoup } from '@/composables/useMediasoup'
import VideoTile from '@/components/conference/VideoTile.vue'

const route = useRoute()
const router = useRouter()
const conferenceStore = useConferenceStore()
const mediasoup = useMediasoup()

const tenantId = computed(() => route.params.tenantId as string)
const conferenceId = computed(() => route.params.conferenceId as string)
const joined = ref(false)
const joining = ref(false)

const statusColor = computed(() => {
  switch (conferenceStore.current?.status) {
    case 'InProgress': return 'success'
    case 'Scheduled': return 'info'
    case 'Ended': return 'grey'
    default: return 'warning'
  }
})

function getDisplayName(userId: string): string {
  const participant = conferenceStore.participants.find((p) => p.user_id === userId)
  return participant?.display_name || userId.slice(0, 8)
}

async function handleJoin() {
  joining.value = true
  try {
    // Always call start to ensure the in-memory mediasoup room exists
    // (it may have been lost on server restart even if DB status is InProgress)
    await conferenceStore.startConference(tenantId.value, conferenceId.value)

    // REST join (adds participant to DB)
    await conferenceStore.joinConference(tenantId.value, conferenceId.value)

    // Fetch conference details
    await conferenceStore.fetchConference(tenantId.value, conferenceId.value)

    // mediasoup signaling join (loads device, connects transports)
    await mediasoup.joinRoom(conferenceId.value)

    // Produce local audio + video
    await mediasoup.produceLocalMedia()

    // Fetch participants for display names
    await conferenceStore.fetchParticipants(tenantId.value, conferenceId.value)

    joined.value = true
  } catch (err) {
    console.error('Failed to join conference:', err)
  } finally {
    joining.value = false
  }
}

async function handleLeave() {
  mediasoup.leaveRoom()
  await conferenceStore.leaveConference(tenantId.value, conferenceId.value)
  joined.value = false
  router.push(`/tenant/${tenantId.value}`)
}

function handleScreenShare() {
  if (mediasoup.isScreenSharing.value) {
    mediasoup.stopScreenShare()
  } else {
    mediasoup.startScreenShare()
  }
}

onMounted(async () => {
  // Load conference info on mount
  try {
    await conferenceStore.fetchConference(tenantId.value, conferenceId.value)
  } catch {
    // Conference not found
  }
})
</script>

<style scoped>
.video-grid {
  max-height: 100%;
  overflow-y: auto;
}
</style>
