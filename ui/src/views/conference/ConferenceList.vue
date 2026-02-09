<template>
  <v-container>
    <v-row>
      <v-col cols="12" class="d-flex align-center">
        <h1 class="text-h4">Conferences</h1>
        <v-spacer />
        <v-btn color="primary" prepend-icon="mdi-plus" @click="showCreate = true">
          Create Conference
        </v-btn>
      </v-col>
    </v-row>

    <v-row>
      <v-col cols="12">
        <v-card v-if="conferenceStore.loading">
          <v-card-text class="text-center">
            <v-progress-circular indeterminate />
          </v-card-text>
        </v-card>

        <v-card v-else-if="conferenceStore.conferences.length === 0">
          <v-card-text class="text-center text-medium-emphasis">
            No conferences yet. Create one to get started.
          </v-card-text>
        </v-card>

        <v-list v-else lines="two">
          <v-list-item
            v-for="conf in conferenceStore.conferences"
            :key="conf.id"
            :to="`/tenant/${tenantId}/conference/${conf.id}`"
          >
            <template #prepend>
              <v-icon>mdi-video</v-icon>
            </template>

            <v-list-item-title>{{ conf.subject }}</v-list-item-title>
            <v-list-item-subtitle>
              {{ formatDate(conf.created_at) }}
              <v-chip size="x-small" class="ml-2" :color="statusColor(conf.status)">
                {{ conf.status }}
              </v-chip>
              <v-chip size="x-small" class="ml-1" variant="outlined">
                {{ conf.participant_count }} participant{{ conf.participant_count !== 1 ? 's' : '' }}
              </v-chip>
            </v-list-item-subtitle>
          </v-list-item>
        </v-list>
      </v-col>
    </v-row>

    <!-- Create conference dialog -->
    <v-dialog v-model="showCreate" max-width="500">
      <v-card>
        <v-card-title>Create Conference</v-card-title>
        <v-card-text>
          <v-text-field v-model="newSubject" label="Subject" required />
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn @click="showCreate = false">{{ $t('common.cancel') }}</v-btn>
          <v-btn color="primary" :disabled="!newSubject.trim()" @click="createConference">
            {{ $t('common.save') }}
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useConferenceStore } from '@/stores/conference'

const route = useRoute()
const router = useRouter()
const conferenceStore = useConferenceStore()

const tenantId = computed(() => route.params.tenantId as string)
const showCreate = ref(false)
const newSubject = ref('')

function statusColor(status: string) {
  switch (status) {
    case 'InProgress': return 'success'
    case 'Scheduled': return 'info'
    case 'Ended': return 'grey'
    default: return 'warning'
  }
}

function formatDate(dateStr: string) {
  if (!dateStr) return ''
  return new Date(dateStr).toLocaleString()
}

async function createConference() {
  const conf = await conferenceStore.createConference(tenantId.value, newSubject.value.trim())
  showCreate.value = false
  newSubject.value = ''
  router.push(`/tenant/${tenantId.value}/conference/${conf.id}`)
}

onMounted(() => {
  conferenceStore.fetchConferences(tenantId.value)
})
</script>
