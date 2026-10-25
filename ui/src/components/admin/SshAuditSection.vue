<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-card variant="outlined">
    <v-card-title class="d-flex align-center">
      <v-icon icon="mdi-console-network-outline" color="primary" class="mr-2" />
      <span>SSH audit</span>
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
      Every SSH session requested on this organization's devices — including
      the ones that were refused. Retained for 90 days.
    </v-card-subtitle>

    <v-card-text>
      <v-alert v-if="error" type="error" variant="tonal" density="compact" class="mb-3">
        {{ error }}
      </v-alert>

      <!-- Says plainly what a row is, because the natural reading of an "SSH
           audit" is a session log and this is not one. The server hands out a
           grant and then steps out of the way. -->
      <v-alert type="info" variant="tonal" density="compact" class="mb-3">
        These are access <strong>decisions</strong>, not session recordings.
        Roomler SSH runs peer-to-peer over the mesh, so the server sees who was
        allowed in and as which account — never what was typed.
      </v-alert>

      <div v-if="!loading && !entries.length" class="text-medium-emphasis text-caption">
        No SSH sessions have been requested yet.
      </div>

      <!-- A card per entry below 600 px (table-stack-xs): the result and the
           command or detail sat about three screens to the right. 20 cards at
           a time, and FR-120 P4: a card shows what decides (the device, when,
           the result) and opens on a tap for who, how, as which account and
           why a refusal. Six labelled lines a card made the page 3.6× its
           desktop height. -->
      <template v-else>
        <v-table density="compact" class="table-stack-xs table-stack-xs--compact">
          <thead>
            <tr>
              <th>When</th>
              <th>Device</th>
              <th>Who</th>
              <th>Via</th>
              <th>As</th>
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
                    :color="e.denied ? 'error' : 'success'"
                    variant="flat"
                    class="flex-shrink-0"
                  >
                    {{ e.denied ? denyLabel(e) : 'granted' }}
                  </v-chip>
                  <v-icon size="small" class="flex-shrink-0" aria-hidden="true">
                    {{ isOpen(rowKey(e, i)) ? 'mdi-chevron-up' : 'mdi-chevron-down' }}
                  </v-icon>
                </button>
              </td>
              <td v-if="!xs" data-label="When" class="text-no-wrap">{{ fmtWhen(e.at) }}</td>
              <td v-if="!xs" data-label="Device" class="text-no-wrap">{{ deviceName(e.agent_id) }}</td>
              <td data-label="Who" class="text-no-wrap stack-detail">{{ userName(e.user_id) }}</td>
              <td data-label="Via" class="stack-detail">
                <v-chip size="x-small" variant="tonal">{{ e.source }}</v-chip>
              </td>
              <td data-label="As" class="text-no-wrap stack-detail">
                <v-chip
                  v-if="asChip(e)"
                  size="x-small"
                  :color="asChip(e)?.color"
                  :title="asChip(e)?.title"
                  variant="tonal"
                >
                  {{ asChip(e)?.label }}
                </v-chip>
                <span v-else class="text-medium-emphasis">—</span>
                <!-- The chip's title, spelled out: a finger never hovers. -->
                <span v-if="xs && asChip(e)?.title" class="text-caption text-medium-emphasis">
                  {{ asChip(e)?.title }}
                </span>
              </td>
              <td v-if="!xs" data-label="Result" class="text-no-wrap">
                <!-- One v-if / v-else pair around the whole refusal: a
                     refusal without a message must never also read
                     "granted". -->
                <v-tooltip v-if="e.denied" :text="e.denied_message ?? ''" location="top">
                  <template #activator="{ props: tip }">
                    <v-chip v-bind="tip" size="x-small" color="error" variant="flat">
                      {{ denyLabel(e) }}
                    </v-chip>
                  </template>
                </v-tooltip>
                <v-chip v-else size="x-small" color="success" variant="flat">granted</v-chip>
              </td>
              <!-- A touch screen never hovers: the server's own reason for a
                   refusal, spelled out in the opened card. -->
              <td v-if="xs && e.denied && e.denied_message" data-label="Why" class="stack-detail">
                {{ e.denied_message }}
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
import { useAgentStore, type SshAuditEntry } from '@/stores/agents'
import { deviceLabel } from '@/composables/useHideDeviceName'
import { useOpenRows } from '@/composables/useOpenRows'
import { usePhoneSlice } from '@/composables/usePhoneSlice'
import { sshAsChip, type SshAsChip } from '@/utils/sshIdentity'

const props = defineProps<{
  tenantId: string
  /** Narrow to one device's history; omit for the whole org. */
  agentId?: string
}>()

const agentStore = useAgentStore()
const entries = ref<SshAuditEntry[]>([])
// FR-116 P3d — a phone shows 20 cards at a time. FR-120 P4 — each shows what
// decides and opens on a tap for the rest.
const { xs } = useDisplay()
const { visible, remaining, nextCount, showMore } = usePhoneSlice(entries)
const { isOpen, toggle } = useOpenRows<string>()
const loading = ref(false)
const error = ref<string | null>(null)

/** Short wording per refusal, so a reader learns WHICH gate said no at a
 *  glance. The server also sends the full sentence as `denied_message`, which
 *  the tooltip shows — this map is only for the chip, and an unknown reason
 *  falls through to the raw value rather than rendering blank. */
const DENY_LABEL: Record<string, string> = {
  org_disabled: 'org switched off',
  no_permission: 'no permission',
  device_disabled: 'device switched off',
  caller_not_allowed: 'caller not allowed',
  origin_not_allowed: 'origin not allowed',
  unsupported: 'agent too old',
  offline: 'device offline',
  no_overlay_address: 'not on the mesh',
  rate_limited: 'rate limited',
  bad_public_key: 'bad public key',
  agent_disabled: 'SSH off on the device',
  grant_expired_on_arrival: 'grant expired (clock skew?)',
  grant_refused: 'device refused grant',
  grant_unconfirmed: 'device did not confirm',
  claim_refused: 'service claim refused',
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

/** FR-105 — a device's SYSTEM service claiming a grant that is not pending
 *  for it (a forged ring) is recorded with no person behind it: the server
 *  writes the all-zero id, which would otherwise render as "00000000". */
const NO_USER = '000000000000000000000000'

function userName(id: string): string {
  if (id === NO_USER) return "device's service"
  return userNames.value[id] ?? id.slice(0, 8)
}

function denyLabel(e: SshAuditEntry): string {
  return (e.denied && DENY_LABEL[e.denied]) || e.denied || 'denied'
}

/** FR-105 — the account the session ran as, from what the grant asked for AND
 *  where the server sent it. `daemon` on the agent's own process is not
 *  "SYSTEM / root" everywhere: on an attended Windows install it is the
 *  signed-in user. SYSTEM-level identities keep the warning colour: they are
 *  worth a second's attention in a list of otherwise ordinary rows. */
function asChip(e: SshAuditEntry): SshAsChip | null {
  return sshAsChip(e.account_mode, e.ran_as)
}

/** A row’s identity: its id, or (an older entry has none) when it happened. */
function rowKey(e: SshAuditEntry, i: number): string {
  return e.id ?? `${e.at}-${i}`
}

function fmtWhen(at: string): string {
  const d = new Date(at)
  return Number.isNaN(d.getTime()) ? at : d.toLocaleString()
}

async function load() {
  loading.value = true
  error.value = null
  try {
    const res = await agentStore.fetchSshAudit(props.tenantId, {
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
