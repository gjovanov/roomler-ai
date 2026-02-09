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
        <v-card>
          <v-card-text class="text-center">
            <v-icon size="48" color="primary">mdi-pound</v-icon>
            <div class="text-h4 mt-2">{{ channelStore.channels.length }}</div>
            <div class="text-subtitle-2">Channels</div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/conferences`" hover>
          <v-card-text class="text-center">
            <v-icon size="48" color="secondary">mdi-video</v-icon>
            <div class="text-h4 mt-2">{{ conferenceStore.conferences.length }}</div>
            <div class="text-subtitle-2">Conferences</div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card>
          <v-card-text class="text-center">
            <v-icon size="48" color="accent">mdi-folder</v-icon>
            <div class="text-h4 mt-2">{{ fileStore.files.length }}</div>
            <div class="text-subtitle-2">Files</div>
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
        <v-btn block color="primary" prepend-icon="mdi-plus" :to="`/tenant/${tenantId}/channels`">
          New Channel
        </v-btn>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-btn block color="secondary" prepend-icon="mdi-video-plus" @click="startMeeting">
          Start Meeting
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
import { useChannelStore } from '@/stores/channels'
import { useConferenceStore } from '@/stores/conference'
import { useFileStore } from '@/stores/files'
import { useTaskStore } from '@/stores/tasks'

const route = useRoute()
const router = useRouter()
const tenantStore = useTenantStore()
const channelStore = useChannelStore()
const conferenceStore = useConferenceStore()
const fileStore = useFileStore()
const taskStore = useTaskStore()

const tenantId = computed(() => route.params.tenantId as string)
const activeTasks = computed(
  () => taskStore.tasks.filter((t) => t.status === 'Processing' || t.status === 'Pending').length,
)

async function startMeeting() {
  const conf = await conferenceStore.createConference(tenantId.value, 'Quick Meeting')
  router.push(`/tenant/${tenantId.value}/conference/${conf.id}`)
}

onMounted(() => {
  if (tenantId.value) {
    channelStore.fetchChannels(tenantId.value)
    conferenceStore.fetchConferences(tenantId.value)
    fileStore.fetchFiles(tenantId.value)
    taskStore.fetchTasks(tenantId.value)
  }
})
</script>
