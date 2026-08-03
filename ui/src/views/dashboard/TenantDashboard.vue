<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <h1 class="text-h5 text-md-h4 mb-1 mb-md-2">{{ tenantStore.current?.name }}</h1>
    <p class="text-subtitle-2 text-md-subtitle-1 text-medium-emphasis mb-2 mb-md-4">Workspace Overview</p>

    <v-row>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/rooms`" hover>
          <v-card-text class="text-center">
            <v-icon size="48" color="primary">mdi-pound</v-icon>
            <div class="text-h4 mt-2">{{ roomStore.rooms.length }}</div>
            <div class="text-subtitle-2">Rooms</div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card>
          <v-card-text class="text-center">
            <v-icon size="48" color="secondary">mdi-video</v-icon>
            <div class="text-h4 mt-2">{{ activeCallCount }}</div>
            <div class="text-subtitle-2">Active Calls</div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/rooms`" hover>
          <v-card-text class="text-center">
            <v-icon size="48" color="accent">mdi-message-text</v-icon>
            <div class="text-h4 mt-2">{{ totalMessageCount }}</div>
            <div class="text-subtitle-2">Messages</div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="showFleet ? `/tenant/${tenantId}/devices` : undefined" :hover="showFleet">
          <v-card-text class="text-center">
            <v-icon size="48" color="warning">mdi-monitor-multiple</v-icon>
            <div class="text-h4 mt-2">{{ onlineDevices }}</div>
            <div class="text-subtitle-2">Devices Online</div>
          </v-card-text>
        </v-card>
      </v-col>
    </v-row>

    <!-- Quick actions -->
    <h2 class="text-h6 mt-4 mb-2">Quick Actions</h2>
    <v-row>
      <v-col cols="12" sm="6" md="3">
        <v-btn block color="primary" prepend-icon="mdi-plus" :to="`/tenant/${tenantId}/rooms`">
          New Room
        </v-btn>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-btn block color="secondary" prepend-icon="mdi-video-plus" @click="startCall">
          Start Call
        </v-btn>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-btn block color="accent" prepend-icon="mdi-upload" :to="`/tenant/${tenantId}/files`">
          Upload File
        </v-btn>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-btn block prepend-icon="mdi-compass" :to="`/tenant/${tenantId}/explore`">
          Explore
        </v-btn>
      </v-col>
    </v-row>

    <!-- Manage: every subsystem reachable from the org hub — the page used
         to dead-end at chat stats with no path to devices/network/admin. -->
    <h2 class="text-h6 mt-4 mb-2">Manage</h2>
    <v-row>
      <v-col v-if="showFleet" cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/devices`" hover>
          <v-card-text>
            <v-icon color="primary" class="mr-2">mdi-monitor-multiple</v-icon>
            <span class="text-subtitle-1 font-weight-medium">Devices</span>
            <div class="text-body-2 text-medium-emphasis mt-1">
              Remote control, enrollment, updates
            </div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col v-if="showFleet" cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/network/machines`" hover>
          <v-card-text>
            <v-icon color="primary" class="mr-2">mdi-lan</v-icon>
            <span class="text-subtitle-1 font-weight-medium">Network</span>
            <div class="text-body-2 text-medium-emphasis mt-1">
              Overlay mesh, tunnels, ACL, routes, DNS
            </div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/admin/members`" hover>
          <v-card-text>
            <v-icon color="primary" class="mr-2">mdi-account-group</v-icon>
            <span class="text-subtitle-1 font-weight-medium">Members &amp; Roles</span>
            <div class="text-body-2 text-medium-emphasis mt-1">
              People, permissions, org settings
            </div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/invites`" hover>
          <v-card-text>
            <v-icon color="primary" class="mr-2">mdi-email-plus</v-icon>
            <span class="text-subtitle-1 font-weight-medium">Invites</span>
            <div class="text-body-2 text-medium-emphasis mt-1">
              Bring teammates into this org
            </div>
          </v-card-text>
        </v-card>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { computed, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useTenantStore } from '@/stores/tenant'
import { useRoomStore } from '@/stores/rooms'
import { useAgentStore } from '@/stores/agents'
import { canSeeFleetNav } from '@/utils/permissions'

const route = useRoute()
const router = useRouter()
const tenantStore = useTenantStore()
const roomStore = useRoomStore()
const agentStore = useAgentStore()

const tenantId = computed(() => route.params.tenantId as string)
const activeCallCount = computed(
  () => roomStore.rooms.filter((r) => r.conference_status === 'in_progress' && (r.participant_count || 0) > 0).length,
)
const totalMessageCount = computed(
  () => roomStore.rooms.reduce((sum, r) => sum + (r.message_count || 0), 0),
)
const onlineDevices = computed(() => agentStore.agents.filter((a) => a.is_online).length)
const showFleet = computed(() => canSeeFleetNav(tenantStore.myPermissions, tenantStore.isOwner))

async function startCall() {
  const now = new Date()
  const name = `Call - ${now.toLocaleDateString()} ${now.toLocaleTimeString()}`
  const room = await roomStore.createRoom(tenantId.value, {
    name,
    has_media: true,
    is_open: true,
  })
  router.push({ name: 'room-call', params: { tenantId: tenantId.value, roomId: room.id } })
}

onMounted(() => {
  if (tenantId.value) {
    roomStore.fetchRooms(tenantId.value)
    if (showFleet.value) agentStore.fetchAgents(tenantId.value).catch(() => {})
  }
})
</script>
