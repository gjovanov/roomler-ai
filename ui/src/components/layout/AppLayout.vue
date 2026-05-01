<template>
  <v-app class="app-layout-root">
    <v-navigation-drawer v-model="drawer" :rail="!mobile && rail" :permanent="!mobile" :temporary="mobile">
      <v-list-item
        :prepend-icon="!mobile && rail ? 'mdi-menu' : undefined"
        :title="!mobile && rail ? '' : 'Roomler'"
        @click="mobile ? undefined : rail = !rail"
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

      <!-- Rooms with unread badges -->
      <v-divider v-if="!rail && roomStore.rooms.length > 0" />
      <v-list v-if="!rail && roomStore.rooms.length > 0" density="compact" nav>
        <v-list-subheader>Rooms</v-list-subheader>
        <v-list-item
          v-for="room in roomStore.rooms"
          :key="room.id"
          :to="`/tenant/${tenantId}/room/${room.id}`"
          :title="room.name"
          :prepend-icon="room.has_media ? 'mdi-video' : 'mdi-pound'"
          density="compact"
        >
          <template #append>
            <v-badge
              v-if="(roomStore.unreadCounts[room.id] || 0) > 0"
              :content="roomStore.unreadCounts[room.id]"
              color="error"
              inline
            />
          </template>
        </v-list-item>
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
      <template #prepend>
        <v-app-bar-nav-icon v-if="mobile" @click="drawer = !drawer" />
      </template>
      <v-app-bar-title class="app-bar-title-truncate">
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
        <!-- Unread messages indicator -->
        <v-btn
          v-if="roomStore.totalUnread > 0"
          size="small"
          variant="tonal"
          color="error"
          class="mr-2"
          @click="goToFirstUnread"
        >
          <v-icon start>mdi-message-badge</v-icon>
          {{ roomStore.totalUnread }}
        </v-btn>
        <v-btn icon="mdi-magnify" size="small" @click="showSearch = true" />
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
            <v-list-item prepend-icon="mdi-account" title="Profile" @click="goToProfile" />
            <v-list-item prepend-icon="mdi-logout" title="Logout" @click="handleLogout" />
          </v-list>
        </v-menu>
      </template>
    </v-app-bar>

    <v-alert
      v-if="wsStore.status === 'connecting'"
      type="warning"
      density="compact"
      variant="tonal"
      closable
      class="ws-status-banner"
    >
      Connecting...
    </v-alert>
    <v-alert
      v-else-if="wsStore.status === 'disconnected'"
      type="error"
      density="compact"
      variant="tonal"
      closable
      class="ws-status-banner"
    >
      Disconnected. Reconnecting...
    </v-alert>

    <v-main class="app-main-no-scroll">
      <router-view />
    </v-main>

    <!-- Global search dialog -->
    <search-dialog v-model="showSearch" />

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
import { useTheme, useDisplay } from 'vuetify'
import { useAuth } from '@/composables/useAuth'
import { useTenantStore } from '@/stores/tenant'
import { useRoomStore } from '@/stores/rooms'
import { useNotificationStore } from '@/stores/notification'
import { useConferenceStore } from '@/stores/conference'
import { useWsStore } from '@/stores/ws'
import NotificationPanel from '@/components/layout/NotificationPanel.vue'
import MiniConference from '@/components/conference/MiniConference.vue'
import SearchDialog from '@/components/layout/SearchDialog.vue'

const { mobile } = useDisplay()
const { auth, logout: handleLogout } = useAuth()
const tenantStore = useTenantStore()
const roomStore = useRoomStore()
const notificationStore = useNotificationStore()
const conferenceStore = useConferenceStore()
const wsStore = useWsStore()
const route = useRoute()
const router = useRouter()
const theme = useTheme()

const isOnCallPage = computed(() => route.name === 'room-call')

// Active calls across all rooms (excluding the one the user is currently in)
const activeCallRooms = computed(() =>
  roomStore.rooms.filter(
    (r) => r.conference_status === 'in_progress' && (r.participant_count || 0) > 0 && r.id !== conferenceStore.roomId,
  ),
)

function goToFirstUnread() {
  // Navigate to the first room with unread messages
  const roomId = Object.entries(roomStore.unreadCounts).find(([, count]) => count > 0)?.[0]
  if (roomId && tenantId.value) {
    router.push(`/tenant/${tenantId.value}/room/${roomId}`)
  }
}

function goToProfile() {
  if (auth.user?.id) {
    router.push({ name: 'profile', params: { userId: auth.user.id } })
  }
}

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
const showSearch = ref(false)

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

function onSearchShortcut(e: KeyboardEvent) {
  if ((e.ctrlKey || e.metaKey) && e.key === 'k') {
    e.preventDefault()
    showSearch.value = true
  }
}

onMounted(async () => {
  await tenantStore.fetchTenants()
  notificationStore.fetchUnreadCount()
  window.addEventListener('room:call_started', onCallStarted)
  window.addEventListener('keydown', onSearchShortcut)
  // Fetch rooms and unread counts for current tenant
  if (tenantId.value) {
    await roomStore.fetchRooms(tenantId.value)
    roomStore.fetchAllUnreadCounts(tenantId.value)
  }
})

onUnmounted(() => {
  window.removeEventListener('room:call_started', onCallStarted)
  window.removeEventListener('keydown', onSearchShortcut)
})
</script>

<style scoped>
/* Neutralize the inner v-application__wrap's min-height: 100vh
   so the layout is constrained to the viewport height provided by the OUTER v-app in App.vue */
.app-layout-root :deep(.v-application__wrap) {
  min-height: 0 !important;
  flex: 1 1 0 !important;
  overflow: hidden;
  height: 100%;
}

/* Make v-main a flex column container so router-view children can fill it,
   and prevent it from growing beyond available space. Use overflow-y: auto
   (rather than hidden) so list-style views — admin / rooms / files / invites
   / billing — get a natural page scrollbar when their content exceeds the
   viewport. Chat / conference views set their own `overflow: hidden` on the
   inner root so their internal scroll containers (`flex-grow-1
   overflow-y-auto` on the message list) keep working unchanged.
   Note: Vuetify 3 does NOT render .v-main__wrap — slot content goes directly in <main>. */
.app-main-no-scroll {
  overflow-y: auto !important;
  flex: 1 1 0 !important;
  min-height: 0 !important;
  display: flex !important;
  flex-direction: column !important;
}

.ws-status-banner {
  flex: 0 0 auto;
  border-radius: 0;
}
.call-pulse {
  animation: pulse-green 2s ease-in-out infinite;
}
@keyframes pulse-green {
  0%, 100% { box-shadow: 0 0 0 0 rgba(76, 175, 80, 0.4); }
  50% { box-shadow: 0 0 0 8px rgba(76, 175, 80, 0); }
}
.app-bar-title-truncate {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
