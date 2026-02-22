<template>
  <v-container>
    <v-row>
      <v-col cols="12">
        <h1 class="text-h4 mb-2">{{ tenantStore.current?.name }}</h1>
        <p class="text-subtitle-1 text-medium-emphasis">Workspace Overview</p>
      </v-col>
    </v-row>

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
        <v-card :to="`/tenant/${tenantId}/files`" hover>
          <v-card-text class="text-center">
            <v-icon size="48" color="accent">mdi-folder</v-icon>
            <div class="text-h4 mt-2">{{ totalMessageCount }}</div>
            <div class="text-subtitle-2">Messages</div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card>
          <v-card-text class="text-center">
            <v-icon size="48" color="warning">mdi-progress-clock</v-icon>
            <div class="text-h4 mt-2">{{ activeTasks }}</div>
            <div class="text-subtitle-2">Active Tasks</div>
          </v-card-text>
        </v-card>
      </v-col>
    </v-row>

    <!-- Quick actions -->
    <v-row class="mt-4">
      <v-col cols="12">
        <h2 class="text-h6 mb-2">Quick Actions</h2>
      </v-col>
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
  </v-container>
</template>

<script setup lang="ts">
import { computed, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useTenantStore } from '@/stores/tenant'
import { useRoomStore } from '@/stores/rooms'
import { useTaskStore } from '@/stores/tasks'

const route = useRoute()
const router = useRouter()
const tenantStore = useTenantStore()
const roomStore = useRoomStore()
const taskStore = useTaskStore()

const tenantId = computed(() => route.params.tenantId as string)
const activeTasks = computed(
  () => taskStore.tasks.filter((t) => t.status === 'Processing' || t.status === 'Pending').length,
)
const activeCallCount = computed(
  () => roomStore.rooms.filter((r) => r.conference_status === 'InProgress').length,
)
const totalMessageCount = computed(
  () => roomStore.rooms.reduce((sum, r) => sum + (r.message_count || 0), 0),
)

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
    taskStore.fetchTasks(tenantId.value)
  }
})
</script>
