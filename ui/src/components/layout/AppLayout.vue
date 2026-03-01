<template>
  <v-app>
    <v-navigation-drawer v-model="drawer" :rail="rail" permanent>
      <v-list-item
        :prepend-icon="rail ? 'mdi-menu' : undefined"
        :title="rail ? '' : 'Roomler'"
        @click="rail = !rail"
      >
        <template v-if="!rail" #prepend>
          <v-icon color="primary">mdi-forum</v-icon>
        </template>
      </v-list-item>

      <v-divider />

      <!-- Tenant selector -->
      <v-list v-if="!rail" density="compact">
        <v-list-item
          v-for="t in tenantStore.tenants"
          :key="t.id"
          :title="t.name"
          :active="tenantStore.current?.id === t.id"
          @click="selectTenant(t)"
          prepend-icon="mdi-domain"
        />
      </v-list>

      <v-divider />

      <!-- Navigation -->
      <v-list density="compact" nav>
        <v-list-item
          v-for="item in navItems"
          :key="item.to"
          :to="item.to"
          :prepend-icon="item.icon"
          :title="item.title"
        />
      </v-list>

      <template #append>
        <!-- Mini conference widget (visible when in call but navigated away) -->
        <mini-conference
          v-if="conferenceStore.isInCall && !isOnCallPage"
        />
        <!-- Pulsing phone icon in rail mode when in call -->
        <v-list v-if="rail && conferenceStore.isInCall" density="compact">
          <v-list-item
            prepend-icon="mdi-phone"
            class="call-indicator"
            @click="returnToCall"
          >
            <v-badge dot color="success" />
          </v-list-item>
        </v-list>
        <v-list density="compact">
          <v-list-item
            prepend-icon="mdi-cog"
            title="Settings"
            :to="settingsRoute"
          />
        </v-list>
      </template>
    </v-navigation-drawer>

    <v-app-bar density="compact" flat>
      <v-app-bar-title>
        {{ pageTitle }}
      </v-app-bar-title>

      <template #append>
        <!-- Active call indicator -->
        <v-menu v-if="activeCallRooms.length > 0">
          <template #activator="{ props: callMenuProps }">
            <v-btn
              v-bind="callMenuProps"
              size="small"
              variant="tonal"
              color="success"
              class="call-pulse mr-2"
            >
              <v-icon start>mdi-phone-ring</v-icon>
              {{ activeCallRooms.length }}
            </v-btn>
          </template>
          <v-list density="compact">
            <v-list-subheader>Active Calls</v-list-subheader>
            <v-list-item
              v-for="room in activeCallRooms"
              :key="room.id"
              :title="room.name"
              :subtitle="`${room.participant_count} participant${room.participant_count !== 1 ? 's' : ''}`"
              prepend-icon="mdi-video"
              @click="router.push({ name: 'room-call', params: { tenantId: tenantId, roomId: room.id } })"
            />
          </v-list>
        </v-menu>
        <v-btn icon="mdi-magnify" size="small" />
        <v-btn
          :icon="isDark ? 'mdi-weather-sunny' : 'mdi-weather-night'"
          size="small"
          @click="toggleTheme"
        />
        <v-menu v-model="showNotifications" :close-on-content-click="false">
          <template #activator="{ props: menuProps }">
            <v-btn icon size="small" v-bind="menuProps">
              <v-badge
                :content="notificationStore.unreadCount"
                :model-value="notificationStore.unreadCount > 0"
                color="error"
                overlap
              >
                <v-icon>mdi-bell-outline</v-icon>
              </v-badge>
            </v-btn>
          </template>
          <notification-panel @close="showNotifications = false" />
        </v-menu>
        <v-menu v-if="auth.isAuthenticated">
          <template #activator="{ props }">
            <v-btn icon v-bind="props" size="small">
              <v-avatar size="28" color="primary">
                <span class="text-caption">{{ initials }}</span>
              </v-avatar>
            </v-btn>
          </template>
          <v-list density="compact">
            <v-list-item prepend-icon="mdi-account" title="Profile" />
            <v-list-item prepend-icon="mdi-logout" title="Logout" @click="handleLogout" />
          </v-list>
        </v-menu>
      </template>
    </v-app-bar>

    <v-main>
      <router-view />
    </v-main>

    <!-- Call started notification -->
    <v-snackbar v-model="callSnackbar" :timeout="8000" color="success" location="top right">
      {{ callSnackbarText }}
      <template #actions>
        <v-btn variant="text" @click="joinCallFromSnackbar">Join</v-btn>
        <v-btn variant="text" icon="mdi-close" @click="callSnackbar = false" />
      </template>
    </v-snackbar>
  </v-app>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useTheme } from 'vuetify'
import { useAuth } from '@/composables/useAuth'
import { useTenantStore } from '@/stores/tenant'
import { useRoomStore } from '@/stores/rooms'
import { useNotificationStore } from '@/stores/notification'
import { useConferenceStore } from '@/stores/conference'
import NotificationPanel from '@/components/layout/NotificationPanel.vue'
import MiniConference from '@/components/conference/MiniConference.vue'

const { auth, logout: handleLogout } = useAuth()
const tenantStore = useTenantStore()
const roomStore = useRoomStore()
const notificationStore = useNotificationStore()
const conferenceStore = useConferenceStore()
const route = useRoute()
const router = useRouter()
const theme = useTheme()

const isOnCallPage = computed(() => route.name === 'room-call')

// Active calls across all rooms (excluding the one the user is currently in)
const activeCallRooms = computed(() =>
  roomStore.rooms.filter(
    (r) => r.conference_status === 'InProgress' && r.id !== conferenceStore.roomId,
  ),
)

function returnToCall() {
  if (conferenceStore.tenantId && conferenceStore.roomId) {
    router.push({
      name: 'room-call',
      params: {
        tenantId: conferenceStore.tenantId,
        roomId: conferenceStore.roomId,
      },
    })
  }
}

const drawer = ref(true)
const rail = ref(false)
const showNotifications = ref(false)

const isDark = computed(() => theme.global.current.value.dark)

function toggleTheme() {
  const next = isDark.value ? 'light' : 'dark'
  theme.global.name.value = next
  localStorage.setItem('roomler-theme', next)
}

// Call notification snackbar
const callSnackbar = ref(false)
const callSnackbarText = ref('')
const callSnackbarRoomId = ref('')

function onCallStarted(e: Event) {
  const detail = (e as CustomEvent).detail as { room_id: string; room_name: string }
  callSnackbarText.value = `Call started in ${detail.room_name}`
  callSnackbarRoomId.value = detail.room_id
  callSnackbar.value = true
}

function joinCallFromSnackbar() {
  callSnackbar.value = false
  if (tenantId.value && callSnackbarRoomId.value) {
    router.push({ name: 'room-call', params: { tenantId: tenantId.value, roomId: callSnackbarRoomId.value } })
  }
}

const tenantId = computed(() => tenantStore.current?.id || '')

const navItems = computed(() => {
  if (!tenantId.value) return []
  const base = `/tenant/${tenantId.value}`
  return [
    { icon: 'mdi-view-dashboard', title: 'Dashboard', to: base },
    { icon: 'mdi-pound', title: 'Rooms', to: `${base}/rooms` },
    { icon: 'mdi-compass', title: 'Explore', to: `${base}/explore` },
    { icon: 'mdi-folder', title: 'Files', to: `${base}/files` },
    { icon: 'mdi-account-plus', title: 'Invites', to: `${base}/invites` },
    { icon: 'mdi-credit-card', title: 'Billing', to: `${base}/billing` },
  ]
})

const settingsRoute = computed(() =>
  tenantId.value ? `/tenant/${tenantId.value}/admin` : '/',
)

const pageTitle = computed(() => {
  const name = route.name as string
  if (name === 'room-chat') return 'Chat'
  if (name === 'room-call') return 'Call'
  return (route.meta.title as string) || 'Roomler'
})

const initials = computed(() => {
  const name = auth.user?.display_name || auth.user?.username || '?'
  return name.charAt(0).toUpperCase()
})

interface Tenant {
  id: string
  name: string
  slug: string
}

function selectTenant(t: Tenant) {
  tenantStore.setCurrent(t as never)
}

onMounted(() => {
  tenantStore.fetchTenants()
  notificationStore.fetchUnreadCount()
  window.addEventListener('room:call_started', onCallStarted)
})

onUnmounted(() => {
  window.removeEventListener('room:call_started', onCallStarted)
})
</script>

<style scoped>
.call-pulse {
  animation: pulse-green 2s ease-in-out infinite;
}
@keyframes pulse-green {
  0%, 100% { box-shadow: 0 0 0 0 rgba(76, 175, 80, 0.4); }
  50% { box-shadow: 0 0 0 8px rgba(76, 175, 80, 0); }
}
</style>
