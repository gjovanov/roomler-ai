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

    <v-card-subtitle class="pb-2">
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

      <v-table v-else density="compact">
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
          <tr v-for="(e, i) in entries" :key="e.id ?? `${e.at}-${i}`">
            <td class="text-no-wrap">{{ fmtWhen(e.at) }}</td>
            <td class="text-no-wrap">{{ deviceName(e.agent_id) }}</td>
            <td class="text-no-wrap">{{ userName(e.user_id) }}</td>
            <td>
              <v-chip size="x-small" variant="tonal">{{ e.source }}</v-chip>
            </td>
            <td class="text-no-wrap">
              <v-chip
                v-if="e.account_mode"
                size="x-small"
                :color="accountColor(e.account_mode)"
                variant="tonal"
              >
                {{ accountLabel(e.account_mode) }}
              </v-chip>
              <span v-else class="text-medium-emphasis">—</span>
            </td>
            <td class="text-no-wrap">
              <v-tooltip v-if="e.denied" :text="e.denied_message ?? ''" location="top">
                <template #activator="{ props: tip }">
                  <v-chip v-bind="tip" size="x-small" color="error" variant="flat">
                    {{ denyLabel(e) }}
                  </v-chip>
                </template>
              </v-tooltip>
              <v-chip v-else size="x-small" color="success" variant="flat">granted</v-chip>
            </td>
          </tr>
        </tbody>
      </v-table>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useAgentStore, type SshAuditEntry } from '@/stores/agents'

const props = defineProps<{
  tenantId: string
  /** Narrow to one device's history; omit for the whole org. */
  agentId?: string
}>()

const agentStore = useAgentStore()
const entries = ref<SshAuditEntry[]>([])
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
}

const ACCOUNT_LABEL: Record<string, string> = {
  console_user: 'signed-in user',
  daemon: 'SYSTEM / root',
  named: 'named account',
}

const deviceNames = computed(() => {
  const m: Record<string, string> = {}
  for (const a of agentStore.agents) m[a.id] = a.name
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
  return userNames.value[id] ?? id.slice(0, 8)
}

function denyLabel(e: SshAuditEntry): string {
  return (e.denied && DENY_LABEL[e.denied]) || e.denied || 'denied'
}

function accountLabel(mode: string): string {
  return ACCOUNT_LABEL[mode] ?? mode
}

/** Root is worth a second's attention in a list of otherwise ordinary rows. */
function accountColor(mode: string): string | undefined {
  return mode === 'daemon' ? 'warning' : undefined
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
