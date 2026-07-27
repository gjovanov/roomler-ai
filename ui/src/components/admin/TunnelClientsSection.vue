<template>
  <v-card>
    <v-card-title class="d-flex align-center">
      <span>Tunnel clients</span>
      <v-spacer />
      <v-btn
        prepend-icon="mdi-key-plus"
        color="primary"
        variant="flat"
        size="small"
        @click="openEnrollDialog"
      >
        Enroll tunnel client
      </v-btn>
    </v-card-title>

    <v-card-text>
      <v-alert
        v-if="store.error"
        type="error"
        variant="tonal"
        closable
        @click:close="store.error = null"
        class="mb-4"
      >
        {{ store.error }}
      </v-alert>

      <p v-if="!store.loading && store.clients.length === 0" class="text-medium-emphasis">
        No tunnel clients enrolled yet. Click "Enroll tunnel client" for a
        one-line installer per platform — the laptop appears here as soon as
        it enrolls.
      </p>

      <div
        v-if="store.loading && store.clients.length === 0"
        class="d-flex justify-center pa-8"
      >
        <v-progress-circular indeterminate />
      </div>

      <v-table v-else-if="store.clients.length > 0" density="compact">
        <thead>
          <tr>
            <th>Name</th>
            <th>Status</th>
            <th>OS</th>
            <th>Version</th>
            <th>Last seen</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="c in store.clients" :key="c.id">
            <td>
              <div class="font-weight-medium">{{ c.name }}</div>
              <div class="text-caption text-medium-emphasis">{{ c.machine_id }}</div>
            </td>
            <td>
              <v-chip
                :color="statusColor(c.status)"
                variant="tonal"
                size="x-small"
                label
              >
                {{ c.status }}
              </v-chip>
            </td>
            <td>{{ c.os }}</td>
            <td>{{ c.client_version || '—' }}</td>
            <td class="text-caption text-medium-emphasis">
              {{ formatLastSeen(c.last_seen_at) }}
            </td>
          </tr>
        </tbody>
      </v-table>
    </v-card-text>

    <!-- S4 — unified enrollment dialog (token + per-OS install commands
         derived from THIS origin; the old dialog hardcoded the prod URL
         and the retired roomler-tunnel binary name). -->
    <EnrollmentDialog
      :model-value="enrollDialog"
      kind="tunnel"
      :token="issuedToken?.enrollment_token ?? null"
      :expires-in="issuedToken?.expires_in ?? null"
      :loading="issuing"
      :error="issueError"
      @update:model-value="(v: boolean) => { if (!v) closeEnrollDialog() }"
    />
  </v-card>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useTunnelClientStore, type TunnelEnrollmentToken } from '@/stores/tunnelClients'
import EnrollmentDialog from '@/components/enroll/EnrollmentDialog.vue'

const props = defineProps<{ tenantId: string }>()

const store = useTunnelClientStore()

const enrollDialog = ref(false)
const issuing = ref(false)
const issuedToken = ref<TunnelEnrollmentToken | null>(null)
const issueError = ref<string | null>(null)

async function openEnrollDialog() {
  enrollDialog.value = true
  issuing.value = true
  issuedToken.value = null
  issueError.value = null
  try {
    issuedToken.value = await store.issueEnrollmentToken(props.tenantId)
  } catch (e) {
    issueError.value = (e as Error).message
  } finally {
    issuing.value = false
  }
}

function closeEnrollDialog() {
  enrollDialog.value = false
  issuedToken.value = null
}

function statusColor(status: string) {
  switch (status) {
    case 'online': return 'success'
    case 'offline': return 'grey'
    case 'quarantined': return 'error'
    default: return 'warning'
  }
}

function formatLastSeen(iso: string): string {
  if (!iso) return '—'
  try {
    return new Date(iso).toLocaleString()
  } catch {
    return iso
  }
}

onMounted(() => {
  store.fetchTunnelClients(props.tenantId)
})
</script>
