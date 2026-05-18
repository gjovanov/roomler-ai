<template>
  <v-card>
    <v-card-title class="d-flex align-center">
      <span>Tunnel Clients</span>
      <v-spacer />
      <v-btn
        prepend-icon="mdi-key-plus"
        color="primary"
        variant="flat"
        size="small"
        @click="openEnrollDialog"
      >
        Issue enrollment token
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
        No tunnel clients enrolled yet. Issue an enrollment token and run
        <code>roomler-tunnel enroll&nbsp;--server&nbsp;…&nbsp;--token&nbsp;…&nbsp;--name&nbsp;"My laptop"</code>
        on the laptop you want to enroll.
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

    <!-- Issue-enrollment-token dialog. The token is displayed once;
         we never persist or re-show it (single-use, 10 min TTL). -->
    <v-dialog v-model="enrollDialog" max-width="640">
      <v-card>
        <v-card-title>Enrollment token</v-card-title>
        <v-card-text>
          <v-alert v-if="issuing" type="info" variant="tonal" class="mb-4">
            Generating…
          </v-alert>
          <template v-else-if="issuedToken">
            <p class="mb-2">
              Paste this command on the laptop that should be enrolled. The
              token expires in {{ Math.round(issuedToken.expires_in / 60) }} minutes
              and may only be used once.
            </p>
            <v-textarea
              :model-value="enrollCommand"
              readonly
              variant="outlined"
              rows="3"
              class="mb-2"
              auto-grow
            />
            <v-btn
              prepend-icon="mdi-content-copy"
              size="small"
              variant="tonal"
              @click="copyCommand"
            >
              Copy command
            </v-btn>
          </template>
          <v-alert v-else-if="issueError" type="error" variant="tonal">
            {{ issueError }}
          </v-alert>
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn @click="enrollDialog = false">Close</v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </v-card>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useTunnelClientStore, type TunnelEnrollmentToken } from '@/stores/tunnelClients'

const props = defineProps<{ tenantId: string }>()

const store = useTunnelClientStore()

const enrollDialog = ref(false)
const issuing = ref(false)
const issuedToken = ref<TunnelEnrollmentToken | null>(null)
const issueError = ref<string | null>(null)

const enrollCommand = computed(() => {
  if (!issuedToken.value) return ''
  return `roomler-tunnel enroll --server https://roomler.ai --token ${issuedToken.value.enrollment_token} --name "My laptop"`
})

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

function copyCommand() {
  navigator.clipboard.writeText(enrollCommand.value).catch(() => {
    issueError.value = 'Could not copy to clipboard — select and copy manually.'
  })
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
