<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-card variant="outlined">
    <v-card-title class="d-flex align-center">
      <v-icon icon="mdi-clipboard-text-clock-outline" color="primary" class="mr-2" />
      <span>Command audit</span>
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
      Every remote command attempted on this organization's devices — including
      the ones that were refused. Retained for 90 days.
    </v-card-subtitle>

    <v-card-text>
      <v-alert v-if="error" type="error" variant="tonal" density="compact" class="mb-3">
        {{ error }}
      </v-alert>

      <div v-if="!loading && !entries.length" class="text-medium-emphasis text-caption">
        No commands have been attempted yet.
      </div>

      <!-- A card per entry below 600 px (table-stack-xs): the result and the
           command or detail sat about three screens to the right. Compact
           (labels beside short values), 20 cards at a time, and a long
           command clamped until tapped: 100 full cards were 43,398 px. -->
      <template v-else>
        <v-table density="compact" class="table-stack-xs table-stack-xs--compact">
          <thead>
            <tr>
              <th>When</th>
              <th>Device</th>
              <th>Who</th>
              <th>Via</th>
              <th>Command</th>
              <th>Result</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="e in visible" :key="e.request_id">
              <td data-label="When" class="text-no-wrap">{{ fmtWhen(e.at) }}</td>
              <td data-label="Device" class="text-no-wrap">{{ deviceName(e.agent_id) }}</td>
              <td data-label="Who" class="text-no-wrap">{{ userName(e.user_id) }}</td>
              <td data-label="Via">
                <v-chip size="x-small" variant="tonal">{{ e.source }}</v-chip>
              </td>
              <td data-label="Command" class="stack-block">
                <code
                  class="audit-cmd"
                  :class="{ 'audit-cmd--open': openCommands.has(e.request_id) }"
                  :title="e.command"
                >{{ e.command }}</code>
                <button
                  v-if="xs && isLongCommand(e.command)"
                  type="button"
                  class="audit-more text-caption"
                  :aria-expanded="openCommands.has(e.request_id)"
                  @click="toggleCommand(e.request_id)"
                >
                  {{ openCommands.has(e.request_id) ? 'Show less' : 'Show all' }}
                </button>
              </td>
              <td data-label="Result" class="text-no-wrap">
                <v-chip size="x-small" :color="resultColor(e)" variant="flat">
                  {{ resultLabel(e) }}
                </v-chip>
                <v-chip
                  v-if="ranAsLabel(e)"
                  size="x-small"
                  variant="outlined"
                  class="ml-1"
                  :title="ranAsTitle(e)"
                >
                  {{ ranAsLabel(e) }}
                </v-chip>
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
import { useAgentStore, type ExecAuditEntry } from '@/stores/agents'
import { deviceLabel } from '@/composables/useHideDeviceName'
import { usePhoneSlice } from '@/composables/usePhoneSlice'

const props = defineProps<{
  tenantId: string
  /** Narrow to one device's history; omit for the whole org. */
  agentId?: string
}>()

const agentStore = useAgentStore()
const entries = ref<ExecAuditEntry[]>([])
const loading = ref(false)
const error = ref<string | null>(null)

// FR-116 P3d — a phone shows 20 cards at a time, each command clamped to
// three lines until tapped (`title` is the desktop's way to the whole of it,
// and a touch screen has no hover).
const { xs } = useDisplay()
const { visible, remaining, nextCount, showMore } = usePhoneSlice(entries)
const openCommands = ref(new Set<string>())
function isLongCommand(cmd: string): boolean {
  return cmd.length > 120 || cmd.includes('\n')
}
function toggleCommand(id: string) {
  const next = new Set(openCommands.value)
  if (next.has(id)) next.delete(id)
  else next.add(id)
  openCommands.value = next
}

/** Human wording for each refusal, so a reader learns WHICH gate said no.
 *  "Denied" alone would throw away the only useful part of the record. */
const DENY_LABEL: Record<string, string> = {
  org_disabled: 'org switched off',
  no_permission: 'no permission',
  device_disabled: 'device switched off',
  caller_not_allowed: 'caller not allowed',
  shell_not_allowed: 'shell not allowed',
  origin_not_allowed: 'origin not allowed',
  unsupported: 'agent too old',
  offline: 'device offline',
  consent_denied: 'consent denied',
  rate_limited: 'rate limited',
  agent_disabled: 'disabled on device',
  system_required: 'SYSTEM required',
  claim_refused: 'claim refused',
  run_as_unavailable: 'needs the SYSTEM service',
}

/** FR-94 — the identity the server sent the command to. `worker` stays
 *  unlabelled on purpose: it is every device before FR-94 (root on Linux). */
const RAN_AS_LABEL: Record<string, string> = {
  system: 'SYSTEM',
  console_user: 'as user',
}

function ranAsLabel(e: ExecAuditEntry): string | null {
  return e.ran_as ? (RAN_AS_LABEL[e.ran_as] ?? null) : null
}

function ranAsTitle(e: ExecAuditEntry): string {
  return e.ran_as === 'system'
    ? "Ran as SYSTEM through the device's service"
    : "Ran through the device's service as the signed-in user"
}

const deviceNames = computed(() => {
  const m: Record<string, string> = {}
  for (const a of agentStore.agents) m[a.id] = deviceLabel(a)
  return m
})

const userNames = computed(() => {
  const m: Record<string, string> = {}
  for (const u of agentStore.tenantMembers) m[u.user_id] = u.display_name || u.nickname || u.user_id
  return m
})

function deviceName(id: string): string {
  return deviceNames.value[id] ?? id.slice(0, 8)
}

function userName(id: string): string {
  // FR-94 — a refused claim names no user: the device's service asked.
  if (/^0{24}$/.test(id)) return "device's service"
  return userNames.value[id] ?? id.slice(0, 8)
}

function resultLabel(e: ExecAuditEntry): string {
  if (e.denied) return DENY_LABEL[e.denied] ?? e.denied
  if (e.exit_code === 0) return 'ok'
  return `exit ${e.exit_code ?? '?'}`
}

function resultColor(e: ExecAuditEntry): string {
  if (e.denied) return 'error'
  return e.exit_code === 0 ? 'success' : 'warning'
}

function fmtWhen(at: string): string {
  const d = new Date(at)
  return Number.isNaN(d.getTime()) ? at : d.toLocaleString()
}

async function load() {
  loading.value = true
  error.value = null
  try {
    const res = await agentStore.fetchExecAudit(props.tenantId, {
      agentId: props.agentId,
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
  if (!agentStore.tenantMembers.length) {
    void agentStore.fetchTenantMembers(props.tenantId)
  }
})

defineExpose({ load })
</script>

<style scoped>
.audit-cmd {
  font-size: 0.75rem;
  /* Long one-liners are the norm here; clamp rather than let one row blow
     the table's width out. */
  display: inline-block;
  max-width: 42ch;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  vertical-align: bottom;
}
/* A phone stacks the row into a card (table-stack-xs), so the command has
   the card's whole width and nothing to blow out. It shows three lines; a
   longer one opens on "Show all" (`title`, above, is the desktop's way in,
   and a touch screen has no hover). A script showed in full ran to
   1,936 px in one card. */
@media (max-width: 599.98px) {
  .audit-cmd {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 3;
    line-clamp: 3;
    max-width: 100%;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .audit-cmd--open {
    display: block;
    -webkit-line-clamp: unset;
    line-clamp: none;
  }
}
.audit-more {
  display: block;
  margin-top: 2px;
  padding: 4px 0;
  border: 0;
  background: none;
  color: rgb(var(--v-theme-primary));
  cursor: pointer;
}
.audit-more:focus-visible {
  outline: 2px solid rgb(var(--v-theme-primary));
  outline-offset: 2px;
}
</style>
