<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-list density="compact" max-width="400" class="notification-panel">
    <v-list-subheader class="d-flex align-center">
      <span>Notifications</span>
      <v-spacer />
      <v-btn
        v-if="store.unreadCount > 0"
        variant="text"
        size="small"
        @click="store.markAllRead()"
      >
        Mark all read
      </v-btn>
    </v-list-subheader>

    <!-- #2817 - desktop notifications are an opt-in: the browser's prompt
         only ever follows this click, never a page load or a sign-in. -->
    <v-list-item v-if="push === 'default'" class="notification-push-optin">
      <v-list-item-title class="text-body-2 text-wrap">
        Get notified on this device, even when Roomler is closed.
      </v-list-item-title>
      <template #append>
        <v-btn
          size="small"
          variant="tonal"
          color="primary"
          :loading="pushBusy"
          @click.stop="turnOnPush"
        >
          Turn on
        </v-btn>
      </template>
    </v-list-item>
    <v-list-item
      v-else-if="push === 'denied'"
      class="text-caption text-medium-emphasis notification-push-blocked"
    >
      Desktop notifications are blocked for this site in your browser's settings.
    </v-list-item>

    <div v-if="store.loading" class="text-center pa-4">
      <v-progress-circular size="20" indeterminate />
    </div>

    <template v-else-if="store.notifications.length === 0">
      <v-list-item class="text-medium-emphasis text-center">
        No notifications
      </v-list-item>
    </template>

    <template v-else>
      <v-list-item
        v-for="notif in store.notifications"
        :key="notif.id"
        :class="{ 'notification-unread': !notif.is_read }"
        @click="handleClick(notif)"
        lines="two"
      >
        <template #prepend>
          <v-icon
            :color="notif.is_read ? undefined : 'primary'"
            size="small"
          >
            {{ iconFor(notif.notification_type) }}
          </v-icon>
        </template>
        <!-- Wraps: the title is the whole notification; one clipped line
             could only be read by tapping it, which also navigates away. -->
        <v-list-item-title class="text-body-2 text-wrap">{{ notif.title }}</v-list-item-title>
        <v-list-item-subtitle class="text-caption">
          {{ formatTime(notif.created_at) }}
        </v-list-item-subtitle>
      </v-list-item>
    </template>
  </v-list>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useNotificationStore, type Notification } from '@/stores/notification'
import { enablePushNotifications, pushState, type PushState } from '@/composables/usePush'
import { openInAppLink } from '@/utils/inAppLink'

const store = useNotificationStore()
const router = useRouter()

// #2817 - where this browser stands on desktop notifications. Starts as
// `unsupported` so nothing renders until it is known.
const push = ref<PushState>('unsupported')
const pushBusy = ref(false)

async function turnOnPush() {
  pushBusy.value = true
  try {
    push.value = await enablePushNotifications()
  } finally {
    pushBusy.value = false
  }
}

const emit = defineEmits<{
  close: []
}>()

function iconFor(type: string): string {
  switch (type) {
    case 'mention': return 'mdi-at'
    case 'message': return 'mdi-message-text'
    case 'reaction': return 'mdi-emoticon'
    case 'invite': return 'mdi-account-plus'
    case 'call': return 'mdi-phone'
    // FR-90 — an agent session waits for a driver.
    case 'approval_request': return 'mdi-shield-key'
    // FR-52 P5 — someone from outside reached a device.
    case 'external_access': return 'mdi-account-arrow-right-outline'
    default: return 'mdi-bell'
  }
}

function formatTime(iso: string): string {
  const d = new Date(iso)
  const now = new Date()
  const diff = now.getTime() - d.getTime()
  const mins = Math.floor(diff / 60000)
  if (mins < 1) return 'just now'
  if (mins < 60) return `${mins}m ago`
  const hours = Math.floor(mins / 60)
  if (hours < 24) return `${hours}h ago`
  const days = Math.floor(hours / 24)
  return `${days}d ago`
}

function handleClick(notif: Notification) {
  if (!notif.is_read) {
    store.markRead(notif.id)
  }
  // #2841 - never navigate a live viewer or a call away from its session:
  // on those pages the link opens in a new tab.
  if (notif.link) {
    openInAppLink(router, notif.link)
  }
  emit('close')
}

onMounted(async () => {
  store.fetchNotifications()
  push.value = await pushState()
})
</script>

<style scoped>
.notification-panel {
  max-height: 400px;
  overflow-y: auto;
}
.notification-unread {
  background: rgba(var(--v-theme-primary), 0.06);
}
</style>
