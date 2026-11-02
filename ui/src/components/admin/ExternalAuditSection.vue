<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <external-people-card class="mb-4" :tenant-id="tenantId" @changed="load" />
  <v-card variant="outlined">
    <v-card-title class="d-flex align-center">
      <v-icon icon="mdi-account-arrow-right-outline" color="primary" class="mr-2" />
      <span>External access audit</span>
      <v-spacer />
      <v-btn
        icon="mdi-refresh"
        size="small"
        variant="text"
        :loading="loading"
        aria-label="Refresh"
        @click="load"
      />
    </v-card-title>

    <v-card-subtitle class="pb-2 text-wrap">
      Every decision about letting someone from outside this organization onto
      its devices, including the refusals. Retained for 90 days.
    </v-card-subtitle>

    <v-card-text>
      <v-alert v-if="error" type="error" variant="tonal" density="compact" class="mb-3">
        {{ error }}
      </v-alert>

      <!-- Two things a reader would otherwise assume: that a row is a session
           log, and that a quiet log means nobody tried. A wrong password is
           decided in the visitor's browser and the device hears no failure
           (FR-52 §4b), so one wrong guess leaves no row of its own. -->
      <v-alert type="info" variant="tonal" density="compact" class="mb-3">
        These are access <strong>decisions</strong>, not session recordings.
        A wrong password is decided in the visitor's browser, so it leaves no
        row here until the device starts refusing them, which reads
        <strong>too many attempts</strong>. Names are whatever the person calls
        themselves.
      </v-alert>

      <div v-if="!loading && !entries.length" class="text-medium-emphasis text-caption">
        Nobody has asked to come in from outside yet.
      </div>

      <template v-else>
        <v-table density="compact" class="table-stack-xs table-stack-xs--compact">
          <thead>
            <tr>
              <th>When</th>
              <th>Device</th>
              <th>Who</th>
              <th>What</th>
              <th>Result</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="(e, i) in visible"
              :key="rowKey(e, i)"
              :class="{ 'stack-open': isOpen(rowKey(e, i)) }"
            >
              <td v-if="xs">
                <button
                  type="button"
                  class="stack-toggle"
                  :aria-expanded="isOpen(rowKey(e, i))"
                  data-testid="audit-row-toggle"
                  @click="toggle(rowKey(e, i))"
                >
                  <span class="stack-toggle__text">
                    <span class="font-weight-medium">{{ deviceName(e.agent_id) }}</span>
                    <span class="text-caption text-medium-emphasis"> · {{ fmtWhen(e.at) }}</span>
                  </span>
                  <v-chip
                    size="x-small"
                    :color="externalAuditResult(e).color"
                    variant="flat"
                    class="flex-shrink-0"
                  >
                    {{ externalAuditResult(e).label }}
                  </v-chip>
                  <v-icon size="small" class="flex-shrink-0" aria-hidden="true">
                    {{ isOpen(rowKey(e, i)) ? 'mdi-chevron-up' : 'mdi-chevron-down' }}
                  </v-icon>
                </button>
              </td>
              <td v-if="!xs" data-label="When" class="text-no-wrap">{{ fmtWhen(e.at) }}</td>
              <td v-if="!xs" data-label="Device" class="text-no-wrap">{{ deviceName(e.agent_id) }}</td>
              <td data-label="Who" class="stack-detail">
                <span class="text-no-wrap">{{ e.actor || e.user_id.slice(0, 8) }}</span>
                <!-- Two accounts can carry one name: the id tells them apart. -->
                <v-chip
                  v-if="isOutsiderRow(e)"
                  size="x-small"
                  variant="tonal"
                  class="ml-1"
                  :title="`Account ${e.user_id}`"
                >
                  outside · {{ e.user_id.slice(-6) }}
                </v-chip>
                <!-- A bar's row is signed by the admin: who it was about. -->
                <v-chip
                  v-else-if="e.principal"
                  size="x-small"
                  variant="tonal"
                  class="ml-1"
                  :title="`Account ${e.principal}`"
                >
                  about · {{ e.principal.slice(-6) }}
                </v-chip>
              </td>
              <td data-label="What" class="text-no-wrap stack-detail">{{ externalAuditAction(e) }}</td>
              <td v-if="!xs" data-label="Result" class="text-no-wrap">
                <v-chip
                  size="x-small"
                  :color="externalAuditResult(e).color"
                  variant="flat"
                  :title="externalAuditResult(e).why ?? undefined"
                >
                  {{ externalAuditResult(e).label }}
                </v-chip>
              </td>
              <!-- A touch screen never hovers: the reason, spelled out. -->
              <td v-if="xs && externalAuditResult(e).why" data-label="Why" class="stack-detail">
                {{ externalAuditResult(e).why }}
              </td>
            </tr>
          </tbody>
        </v-table>
        <div v-if="remaining" class="text-center mt-2">
          <v-btn variant="tonal" size="small" data-testid="audit-show-more" @click="showMore">
            Show {{ nextCount }} more
          </v-btn>
        </div>
      </template>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useDisplay } from 'vuetify'
import ExternalPeopleCard from '@/components/admin/ExternalPeopleCard.vue'
import { useAgentStore } from '@/stores/agents'
import { useNodeLabel } from '@/composables/useNodeLabel'
import { useOpenRows } from '@/composables/useOpenRows'
import { usePhoneSlice } from '@/composables/usePhoneSlice'
import {
  externalAuditAction,
  externalAuditResult,
  fmtWhen,
  isOutsiderRow,
  type ExternalAuditEntry,
} from '@/utils/externalAudit'

const props = defineProps<{ tenantId: string }>()

const agentStore = useAgentStore()
const { agentLabel, ensureLoaded } = useNodeLabel()
const entries = ref<ExternalAuditEntry[]>([])
const { xs } = useDisplay()
const { visible, remaining, nextCount, showMore } = usePhoneSlice(entries)
const { isOpen, toggle } = useOpenRows<string>()
const loading = ref(false)
const error = ref<string | null>(null)

function deviceName(id: string): string {
  return agentLabel(id, id.slice(0, 8))
}

/** A row's identity: its id, or (an older entry has none) when it happened. */
function rowKey(e: ExternalAuditEntry, i: number): string {
  return e.id ?? `${e.at}-${i}`
}

async function load() {
  loading.value = true
  error.value = null
  try {
    const res = await agentStore.fetchExternalAudit(props.tenantId, { perPage: 100 })
    entries.value = res.items
  } catch (e) {
    error.value = (e as Error).message
    entries.value = []
  } finally {
    loading.value = false
  }
}

onMounted(() => {
  void load()
  ensureLoaded(props.tenantId)
})

defineExpose({ load })
</script>
