<template>
  <v-dialog v-model="dialogModel" max-width="600" scrollable>
    <v-card>
      <v-card-title class="d-flex align-center pa-3">
        <v-icon class="mr-2" size="small">mdi-magnify</v-icon>
        <v-text-field
          ref="searchInput"
          v-model="query"
          placeholder="Search messages, rooms, people..."
          variant="plain"
          hide-details
          autofocus
          density="compact"
          @keydown.esc="dialogModel = false"
        />
        <v-chip size="x-small" variant="outlined" class="ml-2">ESC</v-chip>
      </v-card-title>
      <v-divider />
      <v-card-text style="max-height: 400px; overflow-y: auto;" class="pa-0">
        <div v-if="loading" class="text-center pa-4">
          <v-progress-circular indeterminate size="24" />
        </div>
        <div v-else-if="!query" class="text-center pa-4 text-medium-emphasis">
          Type to search across messages, rooms, and people
        </div>
        <div v-else-if="noResults" class="text-center pa-4 text-medium-emphasis">
          No results found for "{{ query }}"
        </div>
        <div v-else>
          <!-- Messages -->
          <div v-if="results.messages.length > 0">
            <div class="text-overline px-4 pt-2">Messages</div>
            <v-list density="compact">
              <v-list-item
                v-for="msg in results.messages"
                :key="msg.id"
                @click="goToMessage(msg)"
              >
                <template #prepend>
                  <v-icon size="small">mdi-message-text</v-icon>
                </template>
                <v-list-item-title class="text-body-2">{{ msg.content_preview }}</v-list-item-title>
                <v-list-item-subtitle>
                  {{ msg.author_name }} in #{{ msg.room_name }} &middot; {{ formatDate(msg.created_at) }}
                </v-list-item-subtitle>
              </v-list-item>
            </v-list>
          </div>
          <!-- Rooms -->
          <div v-if="results.rooms.length > 0">
            <div class="text-overline px-4 pt-2">Rooms</div>
            <v-list density="compact">
              <v-list-item
                v-for="room in results.rooms"
                :key="room.id"
                @click="goToRoom(room)"
              >
                <template #prepend>
                  <v-icon size="small">mdi-pound</v-icon>
                </template>
                <v-list-item-title>{{ room.name }}</v-list-item-title>
                <v-list-item-subtitle v-if="room.purpose">{{ room.purpose }}</v-list-item-subtitle>
              </v-list-item>
            </v-list>
          </div>
          <!-- People -->
          <div v-if="results.users.length > 0">
            <div class="text-overline px-4 pt-2">People</div>
            <v-list density="compact">
              <v-list-item
                v-for="user in results.users"
                :key="user.id"
                @click="goToProfile(user)"
              >
                <template #prepend>
                  <v-avatar size="24" color="primary">
                    <span class="text-caption">{{ (user.display_name || '?')[0].toUpperCase() }}</span>
                  </v-avatar>
                </template>
                <v-list-item-title>{{ user.display_name }}</v-list-item-title>
                <v-list-item-subtitle>@{{ user.username }}</v-list-item-subtitle>
              </v-list-item>
            </v-list>
          </div>
        </div>
      </v-card-text>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { ref, computed, watch } from 'vue'
import { useRouter } from 'vue-router'
import { api } from '@/api/client'
import { useTenantStore } from '@/stores/tenant'

interface SearchMessageResult {
  id: string
  room_id: string
  room_name: string
  author_id: string
  author_name: string
  content_preview: string
  created_at: string
}

interface SearchRoomResult {
  id: string
  name: string
  purpose?: string
  member_count: number
}

interface SearchUserResult {
  id: string
  display_name: string
  username: string
  avatar?: string
}

interface SearchResultsData {
  messages: SearchMessageResult[]
  rooms: SearchRoomResult[]
  users: SearchUserResult[]
}

const props = defineProps<{
  modelValue: boolean
}>()

const emit = defineEmits<{
  'update:modelValue': [value: boolean]
}>()

const dialogModel = computed({
  get: () => props.modelValue,
  set: (val) => emit('update:modelValue', val),
})

const router = useRouter()
const tenantStore = useTenantStore()

const query = ref('')
const loading = ref(false)
const searchInput = ref<HTMLInputElement | null>(null)

const results = ref<SearchResultsData>({
  messages: [],
  rooms: [],
  users: [],
})

const noResults = computed(() => {
  return (
    query.value.trim().length > 0 &&
    !loading.value &&
    results.value.messages.length === 0 &&
    results.value.rooms.length === 0 &&
    results.value.users.length === 0
  )
})

let debounceTimer: ReturnType<typeof setTimeout> | null = null

watch(query, (val) => {
  if (debounceTimer) clearTimeout(debounceTimer)
  const q = val.trim()
  if (!q) {
    results.value = { messages: [], rooms: [], users: [] }
    loading.value = false
    return
  }
  loading.value = true
  debounceTimer = setTimeout(() => {
    doSearch(q)
  }, 300)
})

// Reset state when dialog opens/closes
watch(dialogModel, (val) => {
  if (!val) {
    query.value = ''
    results.value = { messages: [], rooms: [], users: [] }
    loading.value = false
  }
})

async function doSearch(q: string) {
  const tenantId = tenantStore.current?.id
  if (!tenantId) {
    loading.value = false
    return
  }
  try {
    const data = await api.get<SearchResultsData>(
      `/tenant/${tenantId}/search?q=${encodeURIComponent(q)}&limit=20`,
    )
    // Only apply results if query hasn't changed
    if (query.value.trim() === q) {
      results.value = data
    }
  } catch {
    // silently ignore search errors
    results.value = { messages: [], rooms: [], users: [] }
  } finally {
    loading.value = false
  }
}

function formatDate(iso: string): string {
  try {
    const d = new Date(iso)
    const now = new Date()
    const diffMs = now.getTime() - d.getTime()
    const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24))
    if (diffDays === 0) {
      return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    }
    if (diffDays < 7) {
      return `${diffDays}d ago`
    }
    return d.toLocaleDateString()
  } catch {
    return iso
  }
}

function goToMessage(msg: SearchMessageResult) {
  const tenantId = tenantStore.current?.id
  if (tenantId) {
    router.push(`/tenant/${tenantId}/room/${msg.room_id}`)
  }
  dialogModel.value = false
}

function goToRoom(room: SearchRoomResult) {
  const tenantId = tenantStore.current?.id
  if (tenantId) {
    router.push(`/tenant/${tenantId}/room/${room.id}`)
  }
  dialogModel.value = false
}

function goToProfile(user: SearchUserResult) {
  router.push({ name: 'profile', params: { userId: user.id } })
  dialogModel.value = false
}
</script>
