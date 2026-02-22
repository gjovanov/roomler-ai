<template>
  <div>
    <v-list-item
      :to="`/tenant/${tenantId}/room/${room.id}`"
      :style="{ paddingLeft: `${depth * 24 + 16}px` }"
      density="compact"
    >
      <template #prepend>
        <v-icon size="small">
          {{ roomIcon }}
        </v-icon>
      </template>

      <v-list-item-title class="text-body-2">
        {{ room.name }}
      </v-list-item-title>

      <template #append>
        <v-badge
          v-if="room.conference_status === 'InProgress'"
          dot
          color="success"
          class="mr-1"
        >
          <v-chip v-if="room.participant_count > 0" size="x-small" color="success" variant="tonal">
            {{ room.participant_count }}
          </v-chip>
        </v-badge>
        <v-chip v-if="room.member_count > 0" size="x-small" variant="text">
          {{ room.member_count }}
        </v-chip>
        <v-menu>
          <template #activator="{ props: menuProps }">
            <v-btn icon="mdi-dots-vertical" size="x-small" variant="text" v-bind="menuProps" @click.prevent />
          </template>
          <v-list density="compact">
            <v-list-item prepend-icon="mdi-plus" @click="handleCreateChild">
              {{ $t('room.createChild') }}
            </v-list-item>
            <v-list-item v-if="room.has_media" prepend-icon="mdi-video" @click="handleStartCall">
              {{ $t('room.startCall') }}
            </v-list-item>
          </v-list>
        </v-menu>
      </template>
    </v-list-item>

    <!-- Children -->
    <template v-if="children.length > 0">
      <room-tree-item
        v-for="child in children"
        :key="child.id"
        :room="child"
        :tenant-id="tenantId"
        :depth="depth + 1"
      />
    </template>
  </div>
</template>

<script setup lang="ts">
import { computed, inject } from 'vue'
import { useRouter } from 'vue-router'
import { useRoomStore, type Room } from '@/stores/rooms'

const props = defineProps<{
  room: Room
  tenantId: string
  depth: number
}>()

const router = useRouter()
const roomStore = useRoomStore()
const createChildRoom = inject<(parentId: string) => void>('createChildRoom')

const children = computed(() => roomStore.childrenOf(props.room.id))

const roomIcon = computed(() => {
  if (!props.room.is_open) return 'mdi-lock'
  if (props.room.has_media) return 'mdi-video'
  if (props.room.parent_id === undefined) return 'mdi-folder'
  return 'mdi-pound'
})

function handleCreateChild() {
  createChildRoom?.(props.room.id)
}

function handleStartCall() {
  router.push({ name: 'room-call', params: { tenantId: props.tenantId, roomId: props.room.id } })
}
</script>
