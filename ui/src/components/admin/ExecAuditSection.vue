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

    <v-card-subtitle class="pb-2">
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

      <v-table v-else density="compact">
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
          <tr v-for="e in entries" :key="e.request_id">
            <td class="text-no-wrap">{{ fmtWhen(e.at) }}</td>
            <td class="text-no-wrap">{{ deviceName(e.agent_id) }}</td>
            <td class="text-no-wrap">{{ userName(e.user_id) }}</td>
            <td>
              <v-chip size="x-small" variant="tonal">{{ e.source }}</v-chip>
            </td>
            <td>
              <code class="audit-cmd">{{ e.command }}</code>
            </td>
            <td class="text-no-wrap">
              <v-chip size="x-small" :color="resultColor(e)" variant="flat">
                {{ resultLabel(e) }}
              </v-chip>
            </td>
          </tr>
        </tbody>
      </v-table>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useAgentStore, type ExecAuditEntry } from '@/stores/agents'

const props = defineProps<{
  tenantId: string
  /** Narrow to one device's history; omit for the whole org. */
  agentId?: string
}>()

const agentStore = useAgentStore()
const entries = ref<ExecAuditEntry[]>([])
const loading = ref(false)
const error = ref<string | null>(null)

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
</style>
