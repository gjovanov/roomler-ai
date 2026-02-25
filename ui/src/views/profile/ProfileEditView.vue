<template>
  <v-container max-width="600">
    <v-card flat>
      <v-card-title>Edit Profile</v-card-title>
      <v-card-text>
        <v-form @submit.prevent="save">
          <v-text-field
            v-model="form.display_name"
            label="Display Name"
            :rules="[v => !!v || 'Required']"
            class="mb-3"
          />

          <v-textarea
            v-model="form.bio"
            label="Bio"
            rows="3"
            counter="500"
            :rules="[v => !v || v.length <= 500 || 'Max 500 characters']"
            class="mb-3"
          />

          <v-text-field
            v-model="form.avatar"
            label="Avatar URL"
            hint="Direct link to an image"
            class="mb-3"
          />

          <v-select
            v-model="form.locale"
            :items="locales"
            label="Language"
            class="mb-3"
          />

          <v-select
            v-model="form.timezone"
            :items="timezones"
            label="Timezone"
            class="mb-3"
          />

          <div class="d-flex ga-3">
            <v-btn variant="text" @click="router.back()">Cancel</v-btn>
            <v-btn
              type="submit"
              color="primary"
              :loading="saving"
            >
              Save
            </v-btn>
          </div>
        </v-form>
      </v-card-text>
    </v-card>
  </v-container>
</template>

<script setup lang="ts">
import { reactive, ref, onMounted } from 'vue'
import { useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useUserStore } from '@/stores/user'

const router = useRouter()
const authStore = useAuthStore()
const userStore = useUserStore()

const saving = ref(false)
const form = reactive({
  display_name: '',
  bio: '',
  avatar: '',
  locale: 'en-US',
  timezone: 'UTC',
})

const locales = ['en-US', 'en-GB', 'de-DE', 'fr-FR', 'es-ES', 'mk-MK']
const timezones = [
  'UTC', 'America/New_York', 'America/Chicago', 'America/Los_Angeles',
  'Europe/London', 'Europe/Berlin', 'Europe/Paris', 'Europe/Skopje',
  'Asia/Tokyo', 'Asia/Shanghai', 'Australia/Sydney',
]

async function save() {
  saving.value = true
  try {
    await userStore.updateProfile({
      display_name: form.display_name || undefined,
      bio: form.bio || undefined,
      avatar: form.avatar || undefined,
      locale: form.locale || undefined,
      timezone: form.timezone || undefined,
    })
    router.back()
  } finally {
    saving.value = false
  }
}

onMounted(async () => {
  const userId = authStore.user?.id
  if (userId) {
    const profile = await userStore.fetchProfile(userId)
    if (profile) {
      form.display_name = profile.display_name
      form.bio = profile.bio || ''
      form.avatar = profile.avatar || ''
    }
  }
})
</script>
