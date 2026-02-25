<template>
  <v-container max-width="600">
    <div v-if="userStore.loading" class="text-center pa-8">
      <v-progress-circular indeterminate />
    </div>
    <v-card v-else-if="userStore.profile" flat>
      <div class="d-flex align-center pa-6">
        <v-avatar size="80" color="primary" class="mr-4">
          <v-img v-if="userStore.profile.avatar" :src="userStore.profile.avatar" />
          <span v-else class="text-h4">{{ initial }}</span>
        </v-avatar>
        <div>
          <h2 class="text-h5">{{ userStore.profile.display_name }}</h2>
          <p class="text-medium-emphasis">@{{ userStore.profile.username }}</p>
          <v-chip
            size="small"
            :color="presenceColor"
            class="mt-1"
          >
            {{ userStore.profile.presence }}
          </v-chip>
        </div>
        <v-spacer />
        <v-btn
          v-if="isOwnProfile"
          variant="tonal"
          prepend-icon="mdi-pencil"
          :to="{ name: 'profile-edit' }"
        >
          Edit
        </v-btn>
      </div>

      <v-divider />

      <v-card-text>
        <div v-if="userStore.profile.bio" class="mb-4">
          <h3 class="text-subtitle-2 text-medium-emphasis mb-1">Bio</h3>
          <p>{{ userStore.profile.bio }}</p>
        </div>

        <div>
          <h3 class="text-subtitle-2 text-medium-emphasis mb-1">Member since</h3>
          <p>{{ formatDate(userStore.profile.created_at) }}</p>
        </div>
      </v-card-text>
    </v-card>
  </v-container>
</template>

<script setup lang="ts">
import { computed, onMounted } from 'vue'
import { useRoute } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useUserStore } from '@/stores/user'

const route = useRoute()
const authStore = useAuthStore()
const userStore = useUserStore()

const userId = computed(() => route.params.userId as string)
const isOwnProfile = computed(() => userId.value === authStore.user?.id)

const initial = computed(() => {
  const name = userStore.profile?.display_name || '?'
  return name.charAt(0).toUpperCase()
})

const presenceColor = computed(() => {
  switch (userStore.profile?.presence) {
    case 'online': return 'success'
    case 'idle': return 'warning'
    case 'dnd': return 'error'
    default: return undefined
  }
})

function formatDate(iso: string): string {
  return new Date(iso).toLocaleDateString(undefined, {
    year: 'numeric',
    month: 'long',
    day: 'numeric',
  })
}

onMounted(() => {
  userStore.fetchProfile(userId.value)
})
</script>
