<template>
  <v-card>
    <v-card-title class="d-flex align-center">
      <span>Remote-Control Agents</span>
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
        v-if="agentStore.error"
        type="error"
        variant="tonal"
        closable
        @click:close="agentStore.error = null"
        class="mb-4"
      >
        {{ agentStore.error }}
      </v-alert>

      <div
        v-if="agentStore.loading && agentStore.agents.length === 0"
        class="d-flex justify-center pa-8"
      >
        <v-progress-circular indeterminate />
      </div>

      <v-table v-else-if="agentStore.agents.length > 0" density="comfortable">
        <thead>
          <tr>
            <th>Name</th>
            <th>OS</th>
            <th>Version</th>
            <th>Status</th>
            <th>Last seen</th>
            <th class="text-right">Actions</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="a in agentStore.agents" :key="a.id">
            <td>
              <div class="d-flex align-center">
                <v-icon
                  :color="a.is_online ? 'success' : 'grey'"
                  size="small"
                  class="mr-2"
                >
                  {{ a.is_online ? 'mdi-circle' : 'mdi-circle-outline' }}
                </v-icon>
                <span class="font-weight-medium">{{ a.name }}</span>
              </div>
              <div class="text-caption text-medium-emphasis">
                {{ a.machine_id }}
              </div>
            </td>
            <td>
              <v-chip size="x-small" :prepend-icon="osIcon(a.os)" variant="tonal">
                {{ a.os }}
              </v-chip>
            </td>
            <td class="text-caption">{{ a.agent_version || '—' }}</td>
            <td>
              <v-chip size="small" :color="statusColor(a)" variant="flat">
                {{ a.is_online ? 'Online' : a.status }}
              </v-chip>
            </td>
            <td class="text-caption">{{ fmtDate(a.last_seen_at) }}</td>
            <td class="text-right">
              <v-btn
                icon="mdi-delete"
                size="small"
                variant="text"
                color="error"
                @click="confirmDelete(a)"
                :aria-label="`Delete agent ${a.name}`"
              />
            </td>
          </tr>
        </tbody>
      </v-table>

      <div v-else class="text-center pa-8 text-medium-emphasis">
        <v-icon size="64" color="grey-lighten-1" class="mb-2">mdi-desktop-classic</v-icon>
        <p class="mb-2">No agents enrolled yet.</p>
        <p class="text-body-2">
          Issue an enrollment token, run <code>roomler-agent --enroll &lt;token&gt;</code>
          on a machine, and it will appear here.
        </p>
      </div>
    </v-card-text>
  </v-card>

  <!-- Enrollment token dialog -->
  <v-dialog v-model="enrollDialogOpen" max-width="560" persistent>
    <v-card>
      <v-card-title>Enroll a new agent</v-card-title>
      <v-card-text>
        <div v-if="enrollLoading" class="d-flex justify-center pa-4">
          <v-progress-circular indeterminate />
        </div>
        <div v-else-if="enrollToken">
          <p class="text-body-2 mb-3">
            Share this token with the machine you want to enroll. It is single-use
            and expires in {{ Math.round(enrollToken.expires_in / 60) }} minutes.
          </p>
          <v-textarea
            :model-value="enrollToken.enrollment_token"
            readonly
            variant="outlined"
            density="compact"
            rows="4"
            class="font-family-monospace"
            @click="copyTokenToClipboard"
          />
          <v-alert
            v-if="copied"
            type="success"
            variant="tonal"
            density="compact"
            class="mt-2"
          >
            Copied to clipboard
          </v-alert>
          <p class="text-body-2 mt-3 mb-1">On the target machine:</p>
          <pre class="bg-grey-lighten-4 pa-2 rounded text-caption">roomler-agent --enroll &lt;token&gt;</pre>
        </div>
        <div v-else-if="enrollError" class="text-error">
          {{ enrollError }}
        </div>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="closeEnrollDialog">Close</v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

  <!-- Delete confirmation -->
  <v-dialog v-model="deleteDialogOpen" max-width="440">
    <v-card>
      <v-card-title>Delete agent?</v-card-title>
      <v-card-text>
        This will revoke the agent's token and remove it from the list. Any active
        remote-control sessions on this agent will be terminated. This cannot be undone.
        <p class="mt-2 font-weight-medium">{{ deleteTarget?.name }}</p>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="deleteDialogOpen = false">Cancel</v-btn>
        <v-btn color="error" variant="flat" @click="performDelete" :loading="deleting">
          Delete
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { ref, onMounted, watch } from 'vue'
import { useAgentStore, type Agent, type EnrollmentToken } from '@/stores/agents'

const props = defineProps<{ tenantId: string }>()

const agentStore = useAgentStore()

const enrollDialogOpen = ref(false)
const enrollLoading = ref(false)
const enrollToken = ref<EnrollmentToken | null>(null)
const enrollError = ref<string | null>(null)
const copied = ref(false)

const deleteDialogOpen = ref(false)
const deleteTarget = ref<Agent | null>(null)
const deleting = ref(false)

function osIcon(os: string) {
  switch (os) {
    case 'linux': return 'mdi-linux'
    case 'macos': return 'mdi-apple'
    case 'windows': return 'mdi-microsoft-windows'
    default: return 'mdi-desktop-classic'
  }
}

function statusColor(a: Agent) {
  if (a.is_online) return 'success'
  if (a.status === 'quarantined') return 'error'
  return 'grey'
}

function fmtDate(iso: string): string {
  if (!iso) return '—'
  try {
    return new Date(iso).toLocaleString()
  } catch {
    return iso
  }
}

async function openEnrollDialog() {
  enrollDialogOpen.value = true
  enrollLoading.value = true
  enrollToken.value = null
  enrollError.value = null
  copied.value = false
  try {
    enrollToken.value = await agentStore.issueEnrollmentToken(props.tenantId)
  } catch (e) {
    enrollError.value = (e as Error).message
  } finally {
    enrollLoading.value = false
  }
}

function closeEnrollDialog() {
  enrollDialogOpen.value = false
  enrollToken.value = null
  copied.value = false
}

async function copyTokenToClipboard() {
  if (!enrollToken.value) return
  try {
    await navigator.clipboard.writeText(enrollToken.value.enrollment_token)
    copied.value = true
  } catch {
    // ignore — user can copy manually
  }
}

function confirmDelete(a: Agent) {
  deleteTarget.value = a
  deleteDialogOpen.value = true
}

async function performDelete() {
  if (!deleteTarget.value) return
  deleting.value = true
  try {
    await agentStore.deleteAgent(props.tenantId, deleteTarget.value.id)
    deleteDialogOpen.value = false
    deleteTarget.value = null
  } finally {
    deleting.value = false
  }
}

onMounted(() => {
  agentStore.fetchAgents(props.tenantId)
})

watch(() => props.tenantId, (tid) => {
  if (tid) agentStore.fetchAgents(tid)
})
</script>
