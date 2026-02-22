<template>
  <v-container>
    <v-row>
      <v-col cols="12">
        <h1 class="text-h4 mb-4">{{ $t('nav.explore') }}</h1>
        <v-text-field
          v-model="query"
          :label="$t('common.search')"
          prepend-inner-icon="mdi-magnify"
          clearable
          @input="debounceSearch"
        />
      </v-col>
    </v-row>

    <v-row>
      <v-col v-for="room in results" :key="room.id" cols="12" sm="6" md="4">
        <v-card>
          <v-card-title>
            <v-icon class="mr-2" size="small">
              {{ room.is_open ? 'mdi-pound' : 'mdi-lock' }}
            </v-icon>
            {{ room.name }}
          </v-card-title>
          <v-card-subtitle>{{ room.member_count }} members</v-card-subtitle>
          <v-card-text v-if="room.topic?.value">{{ room.topic.value }}</v-card-text>
          <v-card-actions>
            <v-btn color="primary" @click="join(room.id)">{{ $t('room.join') }}</v-btn>
          </v-card-actions>
        </v-card>
      </v-col>
    </v-row>

    <v-row v-if="results.length === 0 && query">
      <v-col cols="12" class="text-center text-medium-emphasis pa-8">
        No rooms found matching "{{ query }}"
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useRoomStore, type Room } from '@/stores/rooms'

const route = useRoute()
const router = useRouter()
const roomStore = useRoomStore()

const tenantId = computed(() => route.params.tenantId as string)
const query = ref('')

const results = ref<Room[]>([])
let searchTimeout: ReturnType<typeof setTimeout> | null = null

function debounceSearch() {
  if (searchTimeout) clearTimeout(searchTimeout)
  searchTimeout = setTimeout(async () => {
    if (query.value.trim()) {
      results.value = await roomStore.explore(tenantId.value, query.value)
    } else {
      results.value = []
    }
  }, 300)
}

async function join(roomId: string) {
  await roomStore.joinRoom(tenantId.value, roomId)
  router.push({ name: 'room-chat', params: { tenantId: tenantId.value, roomId } })
}
</script>
