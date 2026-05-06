<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <div class="d-flex align-center flex-wrap ga-2 mb-2 mb-md-4">
      <h1 class="text-h5 text-md-h4">{{ $t('nav.files') }}</h1>
      <v-spacer />
      <v-btn-toggle v-model="viewMode" mandatory density="compact">
        <v-btn value="all" size="small">All Files</v-btn>
        <v-btn value="room" size="small" :disabled="!currentRoomId">Room Files</v-btn>
      </v-btn-toggle>
      <v-btn color="primary" prepend-icon="mdi-upload" @click="triggerUpload" :disabled="!currentRoomId">
        {{ $t('files.upload') }}
      </v-btn>
      <input
        ref="fileInputRef"
        type="file"
        hidden
        multiple
        @change="handleFileSelect"
      />
    </div>

    <div v-if="fileStore.loading" class="text-center pa-4 pa-md-6">
      <v-progress-circular indeterminate />
    </div>

    <div v-else-if="fileStore.files.length === 0" class="text-center pa-4 pa-md-6 pa-lg-8 text-medium-emphasis">
      {{ $t('files.noFiles') }}
    </div>

    <div v-else class="files-table-wrap">
      <v-table>
          <thead>
            <tr>
              <th>Name</th>
              <th v-if="viewMode === 'all'">Room</th>
              <th>Type</th>
              <th>Size</th>
              <th>Uploaded</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="f in fileStore.files" :key="f.id">
              <td>
                <v-icon size="small" class="mr-2">{{ fileIcon(f.content_type) }}</v-icon>
                {{ f.filename }}
              </td>
              <td v-if="viewMode === 'all'">
                <router-link
                  v-if="f.room_id"
                  :to="{ name: 'room-chat', params: { tenantId: tenantId, roomId: f.room_id } }"
                  class="text-decoration-none"
                >
                  {{ f.room_name || f.room_id }}
                </router-link>
                <span v-else class="text-medium-emphasis">--</span>
              </td>
              <td>{{ f.content_type }}</td>
              <td>{{ formatSize(f.size) }}</td>
              <td>{{ new Date(f.created_at).toLocaleDateString() }}</td>
              <td>
                <v-btn
                  icon="mdi-download"
                  size="small"
                  variant="text"
                  :href="fileStore.downloadUrl(tenantId, f.id)"
                />
                <v-btn
                  icon="mdi-delete"
                  size="small"
                  variant="text"
                  color="error"
                  @click="handleDelete(f.id)"
                />
              </td>
            </tr>
          </tbody>
        </v-table>
    </div>

    <!-- Upload progress -->
    <v-snackbar v-model="uploading" timeout="-1">
      Uploading files...
      <v-progress-linear indeterminate color="primary" />
    </v-snackbar>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, watch } from 'vue'
import { useRoute } from 'vue-router'
import { useFileStore } from '@/stores/files'
import { useRoomStore } from '@/stores/rooms'

const route = useRoute()
const fileStore = useFileStore()
const roomStore = useRoomStore()

const tenantId = computed(() => route.params.tenantId as string)
const currentRoomId = computed(() => roomStore.current?.id || roomStore.rooms[0]?.id || '')
const fileInputRef = ref<HTMLInputElement | null>(null)
const uploading = ref(false)
const viewMode = ref<'all' | 'room'>('all')

function triggerUpload() {
  fileInputRef.value?.click()
}

async function handleFileSelect(event: Event) {
  const input = event.target as HTMLInputElement
  const files = input.files
  if (!files?.length) return

  uploading.value = true
  const roomId = currentRoomId.value
  if (!roomId) return

  for (const file of files) {
    await fileStore.uploadFile(tenantId.value, roomId, file)
  }
  uploading.value = false
  input.value = ''
}

async function handleDelete(fileId: string) {
  await fileStore.deleteFile(tenantId.value, fileId)
}

function fileIcon(contentType: string): string {
  if (contentType.startsWith('image/')) return 'mdi-image'
  if (contentType.startsWith('video/')) return 'mdi-video'
  if (contentType.startsWith('audio/')) return 'mdi-music'
  if (contentType.includes('pdf')) return 'mdi-file-pdf-box'
  if (contentType.includes('spreadsheet') || contentType.includes('excel')) return 'mdi-file-excel'
  if (contentType.includes('document') || contentType.includes('word')) return 'mdi-file-word'
  return 'mdi-file'
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

function loadFiles() {
  if (viewMode.value === 'all') {
    fileStore.fetchTenantFiles(tenantId.value)
  } else {
    const roomId = currentRoomId.value
    if (roomId) {
      fileStore.fetchFiles(tenantId.value, roomId)
    }
  }
}

watch(viewMode, () => {
  loadFiles()
})

onMounted(() => {
  loadFiles()
})
</script>
