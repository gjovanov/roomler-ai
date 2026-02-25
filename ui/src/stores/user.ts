import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

export interface UserProfile {
  id: string
  username: string
  display_name: string
  avatar?: string
  bio?: string
  presence: string
  created_at: string
}

export interface UpdateProfilePayload {
  display_name?: string
  bio?: string
  avatar?: string
  locale?: string
  timezone?: string
}

export const useUserStore = defineStore('user', () => {
  const profile = ref<UserProfile | null>(null)
  const loading = ref(false)

  async function fetchProfile(userId: string) {
    loading.value = true
    try {
      profile.value = await api.get<UserProfile>(`/user/${userId}`)
      return profile.value
    } finally {
      loading.value = false
    }
  }

  async function updateProfile(payload: UpdateProfilePayload) {
    await api.put('/user/me', payload)
    if (profile.value) {
      if (payload.display_name !== undefined) profile.value.display_name = payload.display_name
      if (payload.bio !== undefined) profile.value.bio = payload.bio
      if (payload.avatar !== undefined) profile.value.avatar = payload.avatar
    }
  }

  return {
    profile,
    loading,
    fetchProfile,
    updateProfile,
  }
})
