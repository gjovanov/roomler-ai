<template>
  <v-container>
    <v-row>
      <v-col cols="12" class="d-flex align-center">
        <h1 class="text-h4">{{ $t('nav.rooms') }}</h1>
        <v-spacer />
        <v-btn color="primary" prepend-icon="mdi-plus" @click="showCreate = true">
          {{ $t('room.create') }}
        </v-btn>
      </v-col>
    </v-row>

    <v-row>
      <v-col cols="12">
        <v-card v-if="roomStore.loading">
          <v-card-text class="text-center">
            <v-progress-circular indeterminate />
          </v-card-text>
        </v-card>

        <v-list v-else>
          <template v-for="room in roomStore.rootRooms" :key="room.id">
            <room-tree-item :room="room" :tenant-id="tenantId" :depth="0" />
          </template>
        </v-list>
      </v-col>
    </v-row>

    <!-- Create room dialog -->
    <v-dialog v-model="showCreate" max-width="500">
      <v-card>
        <v-card-title>{{ $t('room.create') }}</v-card-title>
        <v-card-text>
          <v-alert v-if="createError" type="error" closable class="mb-4" @click:close="createError = null">
            {{ createError }}
          </v-alert>
          <v-text-field v-model="newRoom.name" :label="$t('room.name')" required />
          <v-select
            v-model="newRoom.parent_id"
            :items="parentOptions"
            item-title="title"
            item-value="value"
            :label="$t('room.parent')"
            clearable
          />
          <v-checkbox v-model="newRoom.is_open" :label="$t('room.open')" />
          <v-checkbox v-model="newRoom.has_media" :label="$t('room.hasMedia')" />
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn @click="showCreate = false">{{ $t('common.cancel') }}</v-btn>
          <v-btn color="primary" @click="createRoom">{{ $t('common.save') }}</v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, reactive, provide } from 'vue'
import { useRoute } from 'vue-router'
import { useRoomStore } from '@/stores/rooms'
import { ApiError } from '@/api/client'
import RoomTreeItem from '@/components/rooms/RoomTreeItem.vue'

const route = useRoute()
const roomStore = useRoomStore()

const tenantId = computed(() => route.params.tenantId as string)
const showCreate = ref(false)

const newRoom = reactive({
  name: '',
  is_open: true,
  has_media: false,
  parent_id: null as string | null,
})
const createError = ref<string | null>(null)

const parentOptions = computed(() => [
  { title: 'None (root room)', value: null },
  ...roomStore.rooms.map(r => ({ title: r.name, value: r.id })),
])

function openCreateChild(parentId: string) {
  newRoom.parent_id = parentId
  showCreate.value = true
}

provide('createChildRoom', openCreateChild)

async function createRoom() {
  createError.value = null
  try {
    await roomStore.createRoom(tenantId.value, {
      name: newRoom.name,
      is_open: newRoom.is_open,
      has_media: newRoom.has_media,
      parent_id: newRoom.parent_id ?? undefined,
    })
    showCreate.value = false
    newRoom.name = ''
    newRoom.is_open = true
    newRoom.has_media = false
    newRoom.parent_id = null
  } catch (e) {
    if (e instanceof ApiError && typeof (e.data as Record<string, unknown>)?.error === 'string') {
      createError.value = (e.data as Record<string, string>).error
    } else {
      createError.value = (e as Error).message
    }
  }
}

onMounted(() => {
  roomStore.fetchRooms(tenantId.value)
})
</script>
