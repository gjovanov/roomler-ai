<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <div class="d-flex align-center flex-wrap mb-2 mb-md-4" style="gap: 12px">
      <div>
        <h1 class="text-h5 text-md-h4">Growth</h1>
        <p class="text-subtitle-2 text-medium-emphasis mb-0">
          The daily digest — every count is the day, then the trailing 7 and 28 days
        </p>
      </div>
      <v-spacer />
    </div>

    <v-alert v-if="!isPlatformAdmin" type="info" variant="tonal" class="mt-4">
      This page is limited to platform operators.
    </v-alert>

    <template v-else>
      <!-- FR-102 P1d — the Digest view: a day, and the stored sections. -->
      <div class="d-flex align-center flex-wrap mb-3" style="gap: 12px">
        <v-text-field
          v-model="day"
          type="date"
          label="Day (UTC)"
          density="compact"
          hide-details
          style="max-width: 200px"
          :max="yesterday"
        />
        <v-btn size="small" variant="tonal" :loading="loading" @click="load">Load</v-btn>
        <v-btn
          size="small"
          variant="outlined"
          :loading="running"
          :disabled="!day"
          @click="recompute"
        >
          Recompute (no send)
        </v-btn>
        <v-switch
          v-model="includeInternal"
          label="Include internal (live preview)"
          density="compact"
          hide-details
          color="warning"
          @update:model-value="load"
        />
      </div>

      <div v-if="store.digests.length" class="d-flex flex-wrap mb-3" style="gap: 6px">
        <v-chip
          v-for="d in store.digests"
          :key="d.day"
          size="small"
          :variant="d.day === day ? 'elevated' : 'tonal'"
          :color="statusColor(d.status)"
          @click="pick(d.day)"
        >
          {{ d.day }} · {{ d.status }}
        </v-chip>
      </div>

      <v-alert v-if="message" type="info" variant="tonal" density="compact" class="mb-3">
        {{ message }}
      </v-alert>

      <template v-if="sections">
        <div class="text-caption text-medium-emphasis mb-3">
          <template v-if="digest && !preview">
            {{ digest.status }} · computed {{ digest.computed_at ?? '—' }} · sent
            {{ digest.sent_at ?? '—' }} · recipients {{ digest.recipients }} · attempt
            {{ digest.attempts }}
            <template v-if="digest.error"> · {{ digest.error }}</template>
          </template>
          <template v-else-if="preview">
            live preview{{ preview.include_internal ? ' — INTERNAL INCLUDED' : '' }} (not stored,
            not mailed)
          </template>
          · excluded {{ excluded?.users ?? 0 }} users, {{ excluded?.tenants ?? 0 }} orgs
        </div>

        <v-row dense>
          <v-col v-for="(section, key) in sections" :key="key" cols="12" :md="wide(key) ? 12 : 6">
            <v-card>
              <v-card-title class="text-subtitle-1">{{ humanKey(String(key)) }}</v-card-title>
              <v-card-text>
                <growth-node :value="section" />
              </v-card-text>
            </v-card>
          </v-col>
        </v-row>
      </template>
      <v-alert v-else-if="!loading" type="info" variant="tonal">
        No digest for that day yet. Recompute computes and stores it without mailing.
      </v-alert>
    </template>
  </v-container>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useAuthStore } from '@/stores/auth'
import {
  useGrowthStore,
  humanKey,
  type Digest,
  type DigestPreview,
  type DigestStatus,
  type ExcludedCounts,
} from '@/stores/growth'
import GrowthNode from '@/components/growth/GrowthNode.vue'

const authStore = useAuthStore()
const store = useGrowthStore()
const isPlatformAdmin = computed(() => authStore.user?.is_platform_admin === true)

const yesterday = (() => {
  const d = new Date()
  d.setUTCDate(d.getUTCDate() - 1)
  return d.toISOString().slice(0, 10)
})()

const day = ref(yesterday)
const includeInternal = ref(false)
const loading = ref(false)
const running = ref(false)
const message = ref<string | null>(null)
const digest = ref<Digest | null>(null)
const preview = ref<DigestPreview | null>(null)

const sections = computed<Record<string, unknown> | null>(
  () => preview.value?.sections ?? digest.value?.sections ?? null,
)
const excluded = computed<ExcludedCounts | null>(
  () => preview.value?.excluded ?? digest.value?.excluded ?? null,
)

const WIDE = new Set(['signups', 'campaigns', 'website', 'blog', 'newsletter'])
function wide(key: string | number): boolean {
  return WIDE.has(String(key))
}

function statusColor(s: DigestStatus): string {
  if (s === 'sent') return 'success'
  if (s === 'failed') return 'error'
  if (s === 'computing') return 'warning'
  if (s === 'preview') return 'info'
  return 'grey'
}

async function load() {
  if (!isPlatformAdmin.value) return
  loading.value = true
  message.value = null
  try {
    if (includeInternal.value) {
      // The raw totals are never stored: a live preview, every time.
      digest.value = null
      preview.value = await store.fetchPreview(day.value, true)
    } else {
      preview.value = null
      digest.value = await store.fetchDigest(day.value)
    }
  } catch (e) {
    message.value = e instanceof Error ? e.message : 'load failed'
  } finally {
    loading.value = false
  }
}

async function recompute() {
  running.value = true
  message.value = null
  try {
    includeInternal.value = false
    preview.value = null
    digest.value = await store.runDigest(day.value, false)
    message.value = `Recomputed ${day.value} (${digest.value.status}); nothing was mailed.`
    await store.fetchDigests()
  } catch (e) {
    message.value = e instanceof Error ? e.message : 'recompute failed'
  } finally {
    running.value = false
  }
}

async function pick(d: string) {
  day.value = d
  await load()
}

onMounted(async () => {
  if (!isPlatformAdmin.value) return
  await store.fetchDigests()
  if (store.digests.length) day.value = store.digests[0].day
  await load()
})
</script>
