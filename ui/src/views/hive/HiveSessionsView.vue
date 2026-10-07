<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!--
  FR-90 — your agent sessions: start one on a device, see where each stands,
  open its room, stop it. The list is the server's RECORD — the transcript
  itself lives on the device and is read in the session's room.
-->
<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <div class="d-flex align-center mb-2 mb-md-4">
      <h1 class="text-h5 text-md-h4">{{ $t('hive.title') }}</h1>
      <v-spacer />
      <v-btn color="primary" prepend-icon="mdi-plus" data-testid="hive-start-open" @click="openStart">
        {{ $t('hive.start.button') }}
      </v-btn>
    </div>

    <v-card v-if="hive.loading && hive.sessions.length === 0">
      <v-card-text class="text-center"><v-progress-circular indeterminate /></v-card-text>
    </v-card>

    <v-card
      v-else-if="hive.sessions.length === 0"
      variant="outlined"
      class="text-center pa-4 pa-md-6 pa-lg-8"
    >
      <v-icon size="48" color="medium-emphasis" class="mb-2">mdi-robot-outline</v-icon>
      <h2 class="text-h6 mb-2">{{ $t('hive.empty.title') }}</h2>
      <p class="text-body-2 text-medium-emphasis">{{ $t('hive.empty.body') }}</p>
    </v-card>

    <v-list v-else lines="two">
      <v-list-item
        v-for="s in hive.sessions"
        :key="s.id"
        :to="s.room_id ? `/tenant/${tenantId}/room/${s.room_id}` : undefined"
        :data-testid="`hive-session-${s.id}`"
      >
        <template #prepend>
          <v-icon :color="statusColor(s.status)">{{ isLive(s) ? 'mdi-robot' : 'mdi-robot-off-outline' }}</v-icon>
        </template>
        <v-list-item-title>{{ s.title }}</v-list-item-title>
        <v-list-item-subtitle>
          {{ s.device_name || s.device_id }} · {{ s.folder }}
          <span v-if="s.account"> · {{ s.account }}</span>
          <span v-if="s.refusal"> · {{ s.refusal }}</span>
          <span v-else-if="s.end_reason"> · {{ s.end_reason }}</span>
        </v-list-item-subtitle>
        <template #append>
          <v-chip size="small" variant="tonal" :color="statusColor(s.status)" class="mr-2">
            {{ $t(`hive.state.${s.status}`, s.status) }}
          </v-chip>
          <v-btn
            v-if="isLive(s) && s.status !== 'stopping'"
            size="small"
            variant="text"
            color="error"
            :loading="stopping === s.id"
            @click.prevent.stop="stopSession(s.id)"
          >
            {{ $t('hive.stop') }}
          </v-btn>
        </template>
      </v-list-item>
    </v-list>

    <v-dialog v-model="showStart" max-width="520">
      <v-card>
        <v-card-title>{{ $t('hive.start.title') }}</v-card-title>
        <v-card-text>
          <v-autocomplete
            v-model="form.device_id"
            :items="deviceItems"
            :label="$t('hive.start.device')"
            item-title="title"
            item-value="value"
            :loading="devices.loading"
            :no-data-text="$t('hive.start.noDevices')"
            data-testid="hive-start-device"
          />
          <v-text-field
            v-model="form.folder"
            :label="$t('hive.start.folder')"
            :hint="$t('hive.start.folderHint')"
            persistent-hint
            data-testid="hive-start-folder"
          />
          <v-text-field v-model="form.title" :label="$t('hive.start.sessionTitle')" class="mt-2" />
          <v-alert v-if="startError" type="warning" variant="tonal" density="compact" class="mt-2">
            {{ startError }}
          </v-alert>
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn @click="showStart = false">{{ $t('common.cancel') }}</v-btn>
          <v-btn
            color="primary"
            :loading="starting"
            :disabled="!form.device_id || !form.folder.trim()"
            data-testid="hive-start-submit"
            @click="startSession"
          >
            {{ $t('hive.start.submit') }}
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </v-container>
</template>

<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import { isLive, useHiveStore, type HiveSessionStatus } from '@/stores/hive'
import { useDeviceStore } from '@/stores/devices'

const route = useRoute()
const router = useRouter()
const { t } = useI18n()
const hive = useHiveStore()
const devices = useDeviceStore()
const tenantId = computed(() => route.params.tenantId as string)

const showStart = ref(false)
const starting = ref(false)
const startError = ref<string | null>(null)
const stopping = ref<string | null>(null)
const form = reactive({ device_id: '' as string, folder: '', title: '' })

const deviceItems = computed(() =>
  devices.items
    .filter((d) => d.kind === 'agent' && d.presence === 'online')
    .map((d) => ({ title: d.display_name || d.name, value: d.id })),
)

function statusColor(s: HiveSessionStatus): string | undefined {
  switch (s) {
    case 'running':
    case 'awaiting_approval':
      return 'success'
    case 'idle':
    case 'starting':
      return 'info'
    case 'refused':
    case 'lost':
      return 'error'
    default:
      return undefined
  }
}

async function openStart(): Promise<void> {
  startError.value = null
  showStart.value = true
  await devices.fetchDevices(tenantId.value, { kind: 'agent', perPage: 100 })
}

async function startSession(): Promise<void> {
  starting.value = true
  startError.value = null
  try {
    const res = await hive.start(tenantId.value, {
      device_id: form.device_id,
      folder: form.folder.trim(),
      title: form.title.trim() || undefined,
    })
    if (res.outcome === 'refused') {
      startError.value = res.message ?? t('hive.start.refused', { reason: res.reason ?? '' })
      return
    }
    showStart.value = false
    if (res.session?.room_id) {
      await router.push(`/tenant/${tenantId.value}/room/${res.session.room_id}`)
    }
  } catch (e) {
    startError.value = (e as Error).message
  } finally {
    starting.value = false
  }
}

async function stopSession(id: string): Promise<void> {
  stopping.value = id
  try {
    await hive.stop(tenantId.value, id)
  } finally {
    stopping.value = null
  }
}

onMounted(() => hive.fetchSessions(tenantId.value))
</script>
