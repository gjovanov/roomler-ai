<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<!--
  FR-90 P2j-1 — a session's copies, from its room: each device that holds one,
  why (it runs it, the owner's own device, an archive replica), whether it
  took it, and how far behind the primary's its copy is; a move in flight;
  and the owner's Move, to the copy that holds the most.

  Shown only for a record that carries a replicaset: a server that copies
  nothing, and a session placed nowhere, have no copies to show. Freshness is
  the server's own count (`holds`), the rule a move is decided by.
-->
<template>
  <v-dialog v-if="hasCopies" v-model="shown" max-width="600" scrollable>
    <template #activator="{ props: activator }">
      <v-btn
        v-bind="activator"
        size="small"
        variant="text"
        prepend-icon="mdi-content-duplicate"
        data-testid="hive-copies-open"
      >
        {{ $t('hive.roomCopies.button') }}
      </v-btn>
    </template>
    <v-card>
      <v-card-title>{{ $t('hive.roomCopies.title') }}</v-card-title>
      <v-card-text>
        <p class="text-body-2 text-medium-emphasis mb-3">{{ $t('hive.roomCopies.explain') }}</p>
        <v-progress-linear v-if="loading" indeterminate class="mb-2" />
        <v-alert
          v-if="session?.promotion"
          type="info"
          variant="tonal"
          density="compact"
          class="mb-2"
          data-testid="hive-copies-moving"
        >
          {{ movingText }}
        </v-alert>
        <p v-if="shortText" class="text-caption text-medium-emphasis mb-2" data-testid="hive-copies-short">
          {{ shortText }}
        </p>
        <v-list density="compact" data-testid="hive-copies-members">
          <v-list-item
            v-for="m in members"
            :key="m.device_id"
            :data-device="m.device_id"
            :data-role="m.role"
            data-testid="hive-copy"
          >
            <v-list-item-title>{{ nameOf(m.device_id) }}</v-list-item-title>
            <v-list-item-subtitle data-testid="hive-copy-freshness">{{ freshness(m) }}</v-list-item-subtitle>
            <template #append>
              <v-chip size="small" variant="tonal" class="mr-1">
                {{ $t(`hive.roomCopies.role.${m.role}`, m.role) }}
              </v-chip>
              <v-chip size="small" variant="outlined" :color="m.state === 'refused' ? 'error' : undefined">
                {{ stateText(m) }}
              </v-chip>
            </template>
          </v-list-item>
        </v-list>

        <div v-if="isOwner" class="mt-3" data-testid="hive-copies-move">
          <div class="text-subtitle-2">{{ $t('hive.roomCopies.moveTitle') }}</div>
          <p class="text-caption text-medium-emphasis mb-1">{{ $t('hive.roomCopies.moveExplain') }}</p>
          <p v-if="notMovable" class="text-caption text-medium-emphasis" data-testid="hive-copies-not-movable">
            {{ notMovable }}
          </p>
          <template v-else-if="targets.length">
            <v-radio-group v-model="target" density="compact" hide-details>
              <v-radio
                v-for="m in targets"
                :key="m.device_id"
                :value="m.device_id"
                :label="nameOf(m.device_id)"
                :data-testid="`hive-copies-target-${m.device_id}`"
              />
            </v-radio-group>
            <v-text-field
              v-model="folder"
              :label="$t('hive.roomCopies.folder')"
              :hint="$t('hive.roomCopies.folderHint')"
              persistent-hint
              density="compact"
              class="mt-2"
              data-testid="hive-copies-folder"
            />
            <p v-if="ahead" class="text-caption text-warning mt-2" data-testid="hive-copies-ahead">
              {{ $t('hive.roomCopies.ahead', { device: nameOf(ahead.device_id) }) }}
            </p>
            <div class="d-flex justify-end mt-2">
              <v-btn
                color="primary"
                variant="flat"
                size="small"
                :loading="moving"
                :disabled="!target"
                data-testid="hive-copies-move-submit"
                @click="move"
              >
                {{ $t('hive.roomCopies.move') }}
              </v-btn>
            </div>
          </template>
          <p v-else class="text-caption text-medium-emphasis" data-testid="hive-copies-no-target">
            {{ $t('hive.roomCopies.noTarget') }}
          </p>
          <v-alert
            v-if="outcome"
            :type="outcome.kind"
            variant="tonal"
            density="compact"
            class="mt-2"
            data-testid="hive-copies-outcome"
          >
            {{ outcome.text }}
          </v-alert>
        </div>
        <div v-if="error" class="text-caption text-error mt-2" data-testid="hive-copies-error">{{ error }}</div>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="shown = false">{{ $t('common.close') }}</v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { isLive, useHiveStore, type HiveReplicaMember, type HiveSession } from '@/stores/hive'
import { useAgentStore } from '@/stores/agents'
import { useAuthStore } from '@/stores/auth'
import { aheadOfTargets, behind, freshestTarget, moveTargets, primaryOf } from '@/utils/hiveCopies'

const props = defineProps<{ tenantId: string; sessionId: string }>()
const hive = useHiveStore()
const agents = useAgentStore()
const auth = useAuthStore()
const { t } = useI18n()

const shown = ref(false)
const loading = ref(false)
const error = ref<string | null>(null)
/** The record as read when the dialog opened, then as each move answers. */
const read = ref<HiveSession | null>(null)
const target = ref<string | null>(null)
const folder = ref('')
const moving = ref(false)
const outcome = ref<{ kind: 'success' | 'warning' | 'error'; text: string } | null>(null)

const session = computed<HiveSession | null>(
  () => read.value ?? hive.sessions.find((s) => s.id === props.sessionId) ?? null,
)
const hasCopies = computed(() => !!session.value?.replicaset)
const members = computed(() => session.value?.replicaset?.members ?? [])
const primary = computed(() => (session.value ? primaryOf(session.value) : undefined))
const targets = computed(() => (session.value ? moveTargets(session.value) : []))
const ahead = computed(() => (session.value ? aheadOfTargets(session.value) : undefined))
/** Only the owner moves a session; the server says so to anyone else. */
const isOwner = computed(() => !!session.value && !!auth.user && session.value.owner_id === auth.user.id)

/** Why it cannot move now, in words; null when it can. */
const notMovable = computed(() => {
  const s = session.value
  if (!s) return null
  if (s.origin === 'adopted') return t('hive.roomCopies.adoptedStays')
  if (s.promotion) return t('hive.roomCopies.alreadyMoving')
  if (!isLive(s) || s.status === 'stopping') return t('hive.roomCopies.notRunning')
  return null
})

const movingText = computed(() => {
  const p = session.value?.promotion
  if (!p) return ''
  return t('hive.roomCopies.moving', {
    device: p.to_name || nameOf(p.to),
    step: t(`hive.roomCopies.step.${p.state}`, p.state),
  })
})

const shortText = computed(() => {
  const short = session.value?.replicaset?.short
  return short ? t(`hive.roomCopies.short.${short}`, short) : null
})

function nameOf(deviceId: string): string {
  const a = agents.agents.find((x) => x.id === deviceId)
  if (a) return a.display_name || a.name
  if (session.value && deviceId === session.value.device_id && session.value.device_name) {
    return session.value.device_name
  }
  return t('hive.roomCopies.someDevice')
}

function freshness(m: HiveReplicaMember): string {
  if (m.holds === undefined) return t('hive.roomCopies.noTip')
  if (m.role === 'primary') return t('hive.roomCopies.runsIt', { n: m.holds })
  const k = behind(m, primary.value)
  if (k === null) return t('hive.roomCopies.holds', { n: m.holds })
  return k === 0
    ? t('hive.roomCopies.upToDate', { n: m.holds })
    : t('hive.roomCopies.behind', { k, n: m.holds })
}

function stateText(m: HiveReplicaMember): string {
  const word = t(`hive.roomCopies.state.${m.state}`, m.state)
  return m.state === 'refused' && m.refusal ? `${word}: ${m.refusal}` : word
}

async function load(): Promise<void> {
  loading.value = true
  error.value = null
  outcome.value = null
  try {
    const s = await hive.fetchSession(props.tenantId, props.sessionId)
    read.value = s
    folder.value = s.folder
    target.value = freshestTarget(s)?.device_id ?? null
    // Each device by its name: one read per device the page has not seen.
    for (const m of s.replicaset?.members ?? []) {
      if (!agents.agents.some((a) => a.id === m.device_id)) {
        await agents.fetchAgent(props.tenantId, m.device_id).catch(() => {})
      }
    }
  } catch (e) {
    error.value = (e as Error).message
  } finally {
    loading.value = false
  }
}

async function move(): Promise<void> {
  const s = session.value
  if (!s || !target.value || moving.value) return
  moving.value = true
  outcome.value = null
  try {
    const typed = folder.value.trim()
    const res = await hive.promote(props.tenantId, props.sessionId, {
      device_id: target.value,
      // The folder only when it differs: the server keeps the one it runs in.
      ...(typed && typed !== s.folder ? { folder: typed } : {}),
    })
    if (res.session) read.value = res.session
    outcome.value =
      res.outcome === 'refused'
        ? { kind: 'warning', text: res.message ?? t('hive.roomCopies.refused', { reason: res.reason ?? '' }) }
        : { kind: 'success', text: t(`hive.roomCopies.outcome.${res.outcome}`) }
  } catch (e) {
    outcome.value = { kind: 'error', text: (e as Error).message }
  } finally {
    moving.value = false
  }
}

watch(shown, (open) => {
  if (open) void load()
  else read.value = null
})
</script>
