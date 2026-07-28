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
            <!-- Actions leftmost: a right-hand actions column falls off narrow
                 viewports (same fix as AgentsSection). -->
            <th class="text-center" style="width: 56px"></th>
            <th>Name</th>
            <th>Status</th>
            <th>OS</th>
            <th>Version</th>
            <th>Last seen</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="c in store.clients" :key="c.id">
            <td class="text-center">
              <v-btn
                icon="mdi-delete"
                color="error"
                size="small"
                variant="text"
                :aria-label="`Remove tunnel client ${c.name}`"
                @click="confirmDelete(c)"
              />
            </td>
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

    <v-dialog v-model="deleteDialogOpen" max-width="540">
      <v-card>
        <v-card-title>Remove this tunnel client from the tenant?</v-card-title>
        <v-card-text>
          <p class="font-weight-medium mb-3">{{ deleteTarget?.name }}</p>
          <ul class="text-body-2 mb-3 ps-4">
            <li>
              It is evicted from the overlay mesh immediately — peers drop its
              routes, and its live tunnel connection is closed.
            </li>
            <li>
              Its overlay address is released back to this tenant's pool and may
              later be assigned to a <strong>different</strong> machine.
            </li>
            <li>
              The client stays installed on the host. It can be enrolled again,
              but it comes back with a <strong>new overlay address and a new
              name</strong>, so mesh forwards that named it stop resolving.
            </li>
          </ul>
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="deleteDialogOpen = false">Cancel</v-btn>
          <v-btn color="error" variant="flat" :loading="deleting" @click="performDelete">
            Remove tunnel client
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </v-card>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import {
  useTunnelClientStore,
  type TunnelClient,
  type TunnelEnrollmentToken,
} from '@/stores/tunnelClients'
import EnrollmentDialog from '@/components/enroll/EnrollmentDialog.vue'
import { useSnackbar } from '@/composables/useSnackbar'

const props = defineProps<{ tenantId: string }>()

const store = useTunnelClientStore()
const { showSuccess } = useSnackbar()

const deleteDialogOpen = ref(false)
const deleteTarget = ref<TunnelClient | null>(null)
const deleting = ref(false)

function confirmDelete(c: TunnelClient) {
  deleteTarget.value = c
  deleteDialogOpen.value = true
}

async function performDelete() {
  const target = deleteTarget.value
  if (!target) return
  deleting.value = true
  try {
    const res = await store.deleteTunnelClient(props.tenantId, target.id)
    showSuccess(
      res.overlay_ip
        ? `${target.name} removed — ${res.overlay_ip} is back in the pool.`
        : `${target.name} removed.`,
    )
    deleteDialogOpen.value = false
  } catch (e) {
    store.error = (e as Error).message
  } finally {
    deleting.value = false
  }
}

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
