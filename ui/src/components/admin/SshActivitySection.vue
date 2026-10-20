<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-card variant="outlined">
    <v-card-title class="d-flex align-center">
      <v-icon icon="mdi-script-text-outline" color="primary" class="mr-2" />
      <span>SSH activity</span>
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
      What devices reported doing inside their SSH sessions. Retained for 90 days.
    </v-card-subtitle>

    <v-card-text>
      <v-alert v-if="error" type="error" variant="tonal" density="compact" class="mb-3">
        {{ error }}
      </v-alert>

      <!-- Two things a reader will otherwise get wrong, and both matter more
           than anything in the table. Neither is a caveat we can drop later:
           they are properties of the design. -->
      <v-alert type="warning" variant="tonal" density="compact" class="mb-3">
        <strong>An empty list does not mean nothing happened.</strong>
        Reporting is a per-device setting (<code>ssh_activity_log</code>) and is
        off by default, so a device that never opted in looks exactly like an
        idle one. These rows are also the device's own account of itself — a
        compromised host can simply stop talking. The
        <strong>SSH audit</strong> log records every access decision the server
        made, regardless of what the device says afterwards.
      </v-alert>

      <v-alert type="info" variant="tonal" density="compact" class="mb-3">
        Commands are recorded; <strong>session content is not</strong>. There is
        no terminal recording and no command output here — capturing a session
        would mean sending whatever was typed, passwords included, off the
        device. Commands are redacted and truncated before they leave the host.
      </v-alert>

      <div v-if="!loading && !entries.length" class="text-medium-emphasis text-caption">
        No device has reported SSH activity.
      </div>

      <!-- A card per entry below 600 px (table-stack-xs): the result and the
           command or detail sat about three screens to the right. Compact
           (labels beside short values), 20 cards at a time, and a long
           detail clamped until tapped. -->
      <template v-else>
        <v-table density="compact" class="table-stack-xs table-stack-xs--compact">
          <thead>
            <tr>
              <th>When</th>
              <th>Device</th>
              <th>What</th>
              <th>Detail</th>
              <th>Result</th>
              <th>Who</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="(e, i) in visible" :key="e.id ?? `${e.at}-${i}`">
              <td data-label="When" class="text-no-wrap">{{ fmtWhen(e.at) }}</td>
              <td data-label="Device" class="text-no-wrap">{{ deviceName(e.agent_id) }}</td>
              <td data-label="What" class="text-no-wrap">
                <v-chip size="x-small" :color="kindColor(e.kind)" variant="tonal">
                  {{ kindLabel(e.kind) }}
                </v-chip>
              </td>
              <td data-label="Detail" class="text-truncate stack-block" style="max-width: 26rem">
                <template v-if="e.detail">
                  <code
                    class="text-caption activity-detail"
                    :class="{ 'activity-detail--open': openDetails.has(rowKey(e, i)) }"
                    :title="e.detail"
                  >{{ e.detail }}</code>
                  <button
                    v-if="xs && e.detail.length > 120"
                    type="button"
                    class="activity-more text-caption"
                    :aria-expanded="openDetails.has(rowKey(e, i))"
                    @click="toggleDetail(rowKey(e, i))"
                  >
                    {{ openDetails.has(rowKey(e, i)) ? 'Show less' : 'Show all' }}
                  </button>
                </template>
                <span v-else class="text-medium-emphasis">—</span>
              </td>
              <td data-label="Result" class="text-no-wrap">
                <!-- A refused forward is the row an operator most wants to find,
                     so it gets the loud treatment rather than a neutral dash. -->
                <v-chip v-if="!e.allowed" size="x-small" color="error" variant="flat">
                  refused
                </v-chip>
                <v-chip
                  v-else-if="e.exit_code !== null && e.exit_code !== undefined"
                  size="x-small"
                  :color="e.exit_code === 0 ? 'success' : 'warning'"
                  variant="flat"
                >
                  exit {{ e.exit_code }}
                </v-chip>
                <span v-else class="text-medium-emphasis">—</span>
              </td>
              <td data-label="Who" class="text-no-wrap text-caption text-medium-emphasis">
                {{ shortCaller(e.caller) }}
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
import { computed, onMounted, ref } from 'vue'
import { useDisplay } from 'vuetify'
import { useAgentStore, type SshActivityEntry, type SshActivityKind } from '@/stores/agents'
import { deviceLabel } from '@/composables/useHideDeviceName'
import { usePhoneSlice } from '@/composables/usePhoneSlice'

const props = defineProps<{
  tenantId: string
  /** Narrow to one device; omit for the whole org. */
  agentId?: string
  /** Narrow to ONE session — how a reader gets from an audit decision row to
   *  what followed it. */
  grantId?: string
}>()

const agentStore = useAgentStore()
const entries = ref<SshActivityEntry[]>([])
// FR-116 P3d — a phone shows 20 cards at a time, each long detail clamped to
// three lines until tapped (`title` is the desktop's way in; a touch screen
// has no hover).
const { xs } = useDisplay()
const { visible, remaining, nextCount, showMore } = usePhoneSlice(entries)
const openDetails = ref(new Set<string>())
function rowKey(e: SshActivityEntry, i: number): string {
  return e.id ?? `${e.at}-${i}`
}
function toggleDetail(key: string) {
  const next = new Set(openDetails.value)
  if (next.has(key)) next.delete(key)
  else next.add(key)
  openDetails.value = next
}
const loading = ref(false)
const error = ref<string | null>(null)

const KIND_LABEL: Record<SshActivityKind, string> = {
  session_open: 'connected',
  session_close: 'disconnected',
  exec: 'ran command',
  shell: 'opened shell',
  sftp: 'file transfer',
  forward: 'port forward',
}

/** Only the two rows that carry a decision get colour. The session envelope is
 *  bookkeeping and should not compete for attention with what happened inside
 *  it. */
function kindColor(kind: SshActivityKind): string | undefined {
  if (kind === 'exec' || kind === 'shell') return 'primary'
  if (kind === 'forward') return 'info'
  return undefined
}

function kindLabel(kind: SshActivityKind): string {
  return KIND_LABEL[kind] ?? kind
}

const deviceNames = computed(() => {
  const m: Record<string, string> = {}
  for (const a of agentStore.agents) m[a.id] = deviceLabel(a)
  return m
})

function deviceName(id: string): string {
  return deviceNames.value[id] ?? id.slice(0, 8)
}

/** The device sends `ssh:<name>@<overlay addr>:<port>`. The address is noise in
 *  a table this wide, and the name is the part a reader is scanning for — but
 *  an unexpected shape is shown verbatim rather than mangled by a regex that
 *  assumed too much. */
function shortCaller(caller: string): string {
  const m = /^ssh:(.+?)@/.exec(caller)
  return m ? m[1] : caller
}

function fmtWhen(at: string): string {
  const d = new Date(at)
  return Number.isNaN(d.getTime()) ? at : d.toLocaleString()
}

async function load() {
  loading.value = true
  error.value = null
  try {
    const res = await agentStore.fetchSshActivity(props.tenantId, {
      agentId: props.agentId,
      grantId: props.grantId,
      perPage: 100,
    })
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
})

defineExpose({ load })
</script>

<style scoped>
/* A phone stacks the row into a card (table-stack-xs); a long detail shows
   three lines and opens on "Show all". Wider screens keep the one-line
   truncation with the whole of it in `title`. */
@media (max-width: 599.98px) {
  .activity-detail {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 3;
    line-clamp: 3;
    overflow: hidden;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .activity-detail--open {
    display: block;
    -webkit-line-clamp: unset;
    line-clamp: none;
  }
}
.activity-more {
  display: block;
  margin-top: 2px;
  padding: 4px 0;
  border: 0;
  background: none;
  color: rgb(var(--v-theme-primary));
  cursor: pointer;
}
.activity-more:focus-visible {
  outline: 2px solid rgb(var(--v-theme-primary));
  outline-offset: 2px;
}
</style>
