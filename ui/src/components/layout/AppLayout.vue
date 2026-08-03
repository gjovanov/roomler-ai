<template>
  <v-app class="app-layout-root">
    <v-navigation-drawer v-model="drawer" :rail="!mobile && rail" :permanent="!mobile" :temporary="mobile">
      <!-- Brand = way home (org picker at '/'); the chevron owns the rail
           toggle — the brand used to be ONLY a rail toggle, leaving the
           app with no route back to the root at all. -->
      <v-list-item
        v-if="!mobile && rail"
        prepend-icon="mdi-menu"
        title=""
        @click="rail = false"
      />
      <v-list-item v-else title="Roomler" style="cursor: pointer" @click="goHome">
        <template #prepend>
          <v-icon color="primary">mdi-forum</v-icon>
        </template>
        <template v-if="!mobile" #append>
          <v-btn
            icon="mdi-chevron-double-left"
            size="x-small"
            variant="text"
            aria-label="Collapse sidebar"
            @click.stop="rail = true"
          />
        </template>
      </v-list-item>

      <v-divider />

      <!-- Organization switcher: current org + dropdown (switch orgs,
           create a new one, back to the picker). Switching NAVIGATES —
           the old list only mutated the store, leaving the URL and the
           room list on the previous org. -->
      <v-list v-if="!rail" density="compact">
        <v-menu>
          <template #activator="{ props: menuProps }">
            <v-list-item
              v-bind="menuProps"
              :title="tenantStore.current?.name || 'Select organization'"
              prepend-icon="mdi-domain"
              append-icon="mdi-chevron-down"
            />
          </template>
          <v-list density="compact">
            <v-list-item
              v-for="t in tenantStore.tenants"
              :key="t.id"
              :title="t.name"
              :subtitle="t.slug"
              :active="tenantStore.current?.id === t.id"
              prepend-icon="mdi-domain"
              @click="selectTenant(t)"
            />
            <v-divider />
            <v-list-item
              title="New organization"
              prepend-icon="mdi-plus"
              @click="showCreateOrg = true"
            />
            <v-list-item
              title="All organizations"
              prepend-icon="mdi-view-grid-outline"
              @click="goHome"
            />
          </v-list>
        </v-menu>
      </v-list>

      <v-divider />

      <!-- Navigation — S4 pivot IA: fleet first (Devices), then the
           overlay/tunnel Network group, then collaboration (Rooms), with
           the Admin residue last. Two-level via v-list-group; leaf items
           are real routes. -->
      <v-list v-if="tenantId" v-model:opened="openGroups" density="compact" nav>
        <v-list-item
          :to="`/tenant/${tenantId}`"
          prepend-icon="mdi-view-dashboard"
          :title="$t('nav.dashboard')"
          exact
        />
        <v-list-item
          v-if="showFleetNav"
          :to="`/tenant/${tenantId}/devices`"
          prepend-icon="mdi-monitor-multiple"
          :title="$t('nav.devices')"
        />
        <v-list-group v-if="showFleetNav" value="network">
          <template #activator="{ props: groupProps }">
            <v-list-item v-bind="groupProps" prepend-icon="mdi-lan" :title="$t('nav.network')" />
          </template>
          <!-- 2026-08-04 — Machines + Tunnel clients folded into Devices;
               Overlay ACL joins the group (it existed but was reachable
               from NO nav at all). -->
          <v-list-item :to="`/tenant/${tenantId}/network/acl`" prepend-icon="mdi-shield-key" :title="$t('nav.tunnelAcl')" />
          <v-list-item :to="`/tenant/${tenantId}/network/overlay-acl`" prepend-icon="mdi-shield-lock-outline" title="Overlay ACL" />
          <v-list-item :to="`/tenant/${tenantId}/network/subnet-routes`" prepend-icon="mdi-lan-connect" :title="$t('nav.subnetRoutes')" />
          <v-list-item :to="`/tenant/${tenantId}/network/dns`" prepend-icon="mdi-dns" :title="$t('nav.magicDns')" />
        </v-list-group>
        <v-list-group value="rooms">
          <template #activator="{ props: groupProps }">
            <v-list-item v-bind="groupProps" prepend-icon="mdi-forum" :title="$t('nav.collaboration')" />
          </template>
          <v-list-item :to="`/tenant/${tenantId}/rooms`" prepend-icon="mdi-pound" :title="$t('nav.rooms')" />
          <v-list-item :to="`/tenant/${tenantId}/explore`" prepend-icon="mdi-compass" :title="$t('nav.explore')" />
          <v-list-item :to="`/tenant/${tenantId}/files`" prepend-icon="mdi-folder" :title="$t('nav.files')" />
          <v-list-item :to="`/tenant/${tenantId}/invites`" prepend-icon="mdi-account-plus" :title="$t('nav.invites')" />
        </v-list-group>
        <v-list-group value="admin">
          <template #activator="{ props: groupProps }">
            <v-list-item v-bind="groupProps" prepend-icon="mdi-cog" :title="$t('nav.admin')" />
          </template>
          <v-list-item :to="`/tenant/${tenantId}/admin/settings`" prepend-icon="mdi-tune" :title="$t('nav.settings')" />
          <v-list-item :to="`/tenant/${tenantId}/admin/members`" prepend-icon="mdi-account-group" :title="$t('nav.members')" />
          <v-list-item :to="`/tenant/${tenantId}/admin/roles`" prepend-icon="mdi-shield-account" :title="$t('nav.roles')" />
          <v-list-item :to="`/tenant/${tenantId}/admin/tasks`" prepend-icon="mdi-progress-clock" :title="$t('nav.tasks')" />
          <v-list-item :to="`/tenant/${tenantId}/admin/audit-log`" prepend-icon="mdi-clipboard-text-clock" :title="$t('nav.auditLog')" />
          <v-list-item :to="`/tenant/${tenantId}/billing`" prepend-icon="mdi-credit-card" :title="$t('nav.billing')" />
        </v-list-group>
      </v-list>

      <!-- Rooms with unread badges -->
      <v-divider v-if="!rail && roomStore.rooms.length > 0" />
      <v-list v-if="!rail && roomStore.rooms.length > 0" density="compact" nav>
        <v-list-subheader>Your rooms</v-list-subheader>
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

    <!-- New organization dialog (reachable from the org switcher) -->
    <v-dialog v-model="showCreateOrg" max-width="420">
      <v-card>
        <v-card-title>New organization</v-card-title>
        <v-card-text>
          <v-form ref="createOrgForm" @submit.prevent="handleCreateOrg">
            <v-text-field
              v-model="orgName"
              label="Name"
              :rules="[rules.required]"
              @update:model-value="autoSlugFromName"
            />
            <v-text-field
              v-model="orgSlug"
              label="Slug"
              hint="URL-friendly identifier, globally unique"
              :rules="[rules.required, rules.slug]"
              @update:model-value="orgSlugTouched = true"
            />
          </v-form>
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="showCreateOrg = false">Cancel</v-btn>
          <v-btn color="primary" :loading="creatingOrg" @click="handleCreateOrg">Create</v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>

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
import { ref, computed, watch, onMounted, onUnmounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useTheme, useDisplay } from 'vuetify'
import { useAuth } from '@/composables/useAuth'
import { useTenantStore } from '@/stores/tenant'
import { canSeeFleetNav } from '@/utils/permissions'
import { useRoomStore } from '@/stores/rooms'
import { useNotificationStore } from '@/stores/notification'
import { useConferenceStore } from '@/stores/conference'
import { useWsStore } from '@/stores/ws'
import { useMessageStore } from '@/stores/messages'
import NotificationPanel from '@/components/layout/NotificationPanel.vue'
import MiniConference from '@/components/conference/MiniConference.vue'
import SearchDialog from '@/components/layout/SearchDialog.vue'
import { useSnackbar } from '@/composables/useSnackbar'
import { useValidation } from '@/composables/useValidation'

const { mobile } = useDisplay()
const { showError: showCreateOrgError } = useSnackbar()
const { rules } = useValidation()
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

// Drawer starts CLOSED on mobile (so the hamburger button is the
// affordance to open it) and OPEN on desktop (the standard sidebar
// experience). Without this, mobile users would land with the nav
// drawer covering the page until they explicitly close it.
const drawer = ref(!mobile.value)
const rail = ref(false)
// S4 nav groups — Collaboration starts open (the day-to-day pages);
// Network/Admin start collapsed. The user's toggles win afterwards.
const openGroups = ref<string[]>(['rooms'])
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

// Route param wins over the store: a deep link to /tenant/B while the
// store still says A used to point every sidebar target at A.
const tenantId = computed(
  () => (route.params.tenantId as string | undefined) || tenantStore.current?.id || '',
)

// Route → store sync: keep tenantStore.current on the org the URL names
// (nothing watched route.params.tenantId before, so titles/settings/billing
// showed the alphabetically-first org on deep links).
watch(
  () => route.params.tenantId,
  (id) => {
    if (typeof id === 'string' && id && id !== tenantStore.current?.id) {
      const t = tenantStore.tenants.find((x) => x.id === id)
      if (t) tenantStore.setCurrent(t)
    }
  },
)

// S6 — keep the WS's tenant-affinity key in sync with the active
// tenant. The store redials the socket when it actually changes so the
// front LB re-routes this session onto the tenant's pod.
watch(
  () => tenantStore.current?.id ?? null,
  (tid, prevTid) => {
    wsStore.setTenantAffinity(tid)
    // Deferred-S4 — the caller's own permission mask drives fleet-nav
    // visibility. Fail-open: nav shows until the fetch lands (see
    // canSeeFleetNav); the server still enforces every action.
    if (tid) void tenantStore.fetchMyMembership(tid)
    // Org SWITCH (not the immediate first fire — onMounted owns the
    // initial fetch): reload the sidebar's rooms + unread badges for the
    // new org; the old switcher left them showing the previous org.
    if (tid && prevTid !== undefined && prevTid !== tid) {
      void roomStore.fetchRooms(tid).then(() => roomStore.fetchAllUnreadCounts(tid))
    }
  },
  { immediate: true },
)

// Gate the Devices page + Network group on fleet permissions
// (MANAGE_AGENTS / REMOTE_CONTROL / ADMINISTRATOR / owner). Collab +
// Admin groups stay visible for every member.
const showFleetNav = computed(() =>
  canSeeFleetNav(tenantStore.myPermissions, tenantStore.isOwner),
)

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

function goHome() {
  router.push('/')
}

function selectTenant(t: Tenant) {
  tenantStore.setCurrent(t as never)
  // Navigate — the pre-2026-08 switcher only mutated the store, leaving
  // the URL, sidebar targets and room list on the previous org.
  router.push(`/tenant/${t.id}`)
}

// ── New-organization dialog ─────────────────────────────────────
const showCreateOrg = ref(false)
const createOrgForm = ref()
const orgName = ref('')
const orgSlug = ref('')
const orgSlugTouched = ref(false)
const creatingOrg = ref(false)

function slugify(name: string): string {
  return name
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
}

function autoSlugFromName() {
  if (!orgSlugTouched.value) orgSlug.value = slugify(orgName.value)
}

async function handleCreateOrg() {
  const { valid } = await createOrgForm.value.validate()
  if (!valid) return
  creatingOrg.value = true
  try {
    const tenant = await tenantStore.createTenant(orgName.value, orgSlug.value)
    showCreateOrg.value = false
    orgName.value = ''
    orgSlug.value = ''
    orgSlugTouched.value = false
    router.push(`/tenant/${tenant.id}`)
  } catch (e) {
    const msg = e instanceof Error ? e.message : 'Failed to create organization'
    // tenants.slug is globally unique — surface the dup-key case in words.
    showCreateOrgError(
      msg.includes('duplicate') || msg.includes('E11000')
        ? 'That slug is already taken — pick another'
        : msg,
    )
  } finally {
    creatingOrg.value = false
  }
}

function onSearchShortcut(e: KeyboardEvent) {
  if ((e.ctrlKey || e.metaKey) && e.key === 'k') {
    e.preventDefault()
    showSearch.value = true
  }
}

// WS came back after a drop: pushes that would have arrived meanwhile are
// gone — refetch rooms (call badges), unread counts, and the open room's
// messages so the UI converges without a manual reload.
async function onWsReconnected() {
  if (!tenantId.value) return
  await roomStore.fetchRooms(tenantId.value)
  roomStore.fetchAllUnreadCounts(tenantId.value)
  const messageStore = useMessageStore()
  if (messageStore.currentRoomId) {
    messageStore.fetchMessages(tenantId.value, messageStore.currentRoomId)
  }
}

onMounted(async () => {
  await tenantStore.fetchTenants()
  notificationStore.fetchUnreadCount()
  window.addEventListener('room:call_started', onCallStarted)
  window.addEventListener('keydown', onSearchShortcut)
  window.addEventListener('ws:reconnected', onWsReconnected)
  // Fetch rooms and unread counts for current tenant
  if (tenantId.value) {
    await roomStore.fetchRooms(tenantId.value)
    roomStore.fetchAllUnreadCounts(tenantId.value)
  }
})

onUnmounted(() => {
  window.removeEventListener('room:call_started', onCallStarted)
  window.removeEventListener('keydown', onSearchShortcut)
  window.removeEventListener('ws:reconnected', onWsReconnected)
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
