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

      <!-- Desktop / tablet (≥ sm): full table. Action column is leftmost so
           it never falls off the right edge regardless of overflow; codec
           chips collapse to "+N" on lgAndDown to reclaim ~200px on mid-width
           viewports (was the field bug from the field-test host 2026-05-01: "cannot
           select the last Laptop in the list, not possible to scroll").
           Below sm we render the dedicated card list further down. -->
      <v-table
        v-else-if="agentStore.agents.length > 0 && !mobile"
        density="compact"
        class="agents-table"
      >
        <thead>
          <tr>
            <th class="agents-actions-col">Actions</th>
            <th>Name</th>
            <th>Status</th>
            <th>OS</th>
            <th>Codecs</th>
            <th>Last seen</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="a in agentStore.agents" :key="a.id">
            <td class="agents-actions-col">
              <v-btn
                icon="mdi-remote-desktop"
                size="small"
                variant="text"
                color="primary"
                :disabled="!a.is_online"
                :to="{ name: 'agent-remote', params: { tenantId, agentId: a.id } }"
                :aria-label="`Connect to agent ${a.name}`"
              />
              <v-btn
                icon="mdi-alert-circle-outline"
                size="small"
                variant="text"
                color="warning"
                @click="openCrashes(a)"
                :aria-label="`View crash reports for ${a.name}`"
                title="Crash reports"
              />
              <v-btn
                icon="mdi-text-box-search-outline"
                size="small"
                variant="text"
                color="primary"
                @click="openLogs(a)"
                :aria-label="`View logs for ${a.name}`"
                title="Agent logs"
              />
              <v-btn
                icon="mdi-delete"
                size="small"
                variant="text"
                color="error"
                @click="confirmDelete(a)"
                :aria-label="`Delete agent ${a.name}`"
              />
            </td>
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
                <v-btn
                  :icon="copiedAgentId === a.id ? 'mdi-check' : 'mdi-content-copy'"
                  size="x-small"
                  variant="text"
                  :color="copiedAgentId === a.id ? 'success' : undefined"
                  class="ml-1"
                  @click="copyAgentId(a.id)"
                  :aria-label="`Copy agent ID for ${a.name}`"
                  :title="copiedAgentId === a.id
                    ? 'Copied!'
                    : `Copy agent ID — use with \`roomler-tunnel forward --agent <id>\``"
                />
              </div>
              <div class="text-caption text-medium-emphasis d-flex align-center flex-wrap">
                <span class="agent-id-preview" :title="`Agent ID: ${a.id}`">
                  id: {{ shortId(a.id) }}
                </span>
                <span class="mx-1">·</span>
                <span :title="`machine_id: ${a.machine_id}`">{{ shortId(a.machine_id) }}</span>
                <span v-if="a.agent_version"> · v{{ a.agent_version }}</span>
              </div>
            </td>
            <td>
              <v-chip size="small" :color="statusColor(a)" variant="flat">
                {{ a.is_online ? 'Online' : a.status }}
              </v-chip>
            </td>
            <td>
              <v-chip size="x-small" :prepend-icon="osIcon(a.os)" variant="tonal">
                {{ a.os }}
              </v-chip>
            </td>
            <td>
              <div v-if="codecChips(a).length === 0" class="text-caption text-medium-emphasis">—</div>
              <div v-else-if="lgAndDown" class="d-flex flex-wrap gap-1 align-center">
                <v-chip
                  size="x-small"
                  :color="codecChips(a)[0].color"
                  variant="tonal"
                  :title="codecChips(a).map(c => c.tooltip).join(', ')"
                >
                  {{ codecChips(a)[0].label }}
                </v-chip>
                <v-chip
                  v-if="codecChips(a).length > 1"
                  size="x-small"
                  variant="tonal"
                  :title="codecChips(a).slice(1).map(c => c.tooltip).join(', ')"
                >
                  +{{ codecChips(a).length - 1 }}
                </v-chip>
              </div>
              <div v-else class="d-flex flex-wrap gap-1">
                <v-chip
                  v-for="codec in codecChips(a)"
                  :key="codec.label"
                  size="x-small"
                  :color="codec.color"
                  variant="tonal"
                  :title="codec.tooltip"
                >
                  {{ codec.label }}
                </v-chip>
              </div>
            </td>
            <td class="text-caption" :title="fmtDate(a.last_seen_at)">{{ fmtRelative(a.last_seen_at) }}</td>
          </tr>
        </tbody>
      </v-table>

      <!-- Mobile: stacked card list. Each card is a tappable target;
           Connect / Delete actions are full-width buttons at the bottom
           of the card so the rightmost item is reachable on a narrow
           viewport (the field bug from the field-test host 2026-05-01: "cannot
           select the last Laptop in the list, not possible to scroll").
           Codecs / version / last-seen drop to small lines so the
           card stays compact at ~120px tall. -->
      <v-list
        v-else-if="agentStore.agents.length > 0 && mobile"
        density="compact"
        class="pa-0"
      >
        <v-card
          v-for="a in agentStore.agents"
          :key="a.id"
          variant="outlined"
          class="mb-2"
        >
          <v-card-text class="pa-3">
            <div class="d-flex align-center mb-1">
              <v-icon
                :color="a.is_online ? 'success' : 'grey'"
                size="small"
                class="mr-2"
              >
                {{ a.is_online ? 'mdi-circle' : 'mdi-circle-outline' }}
              </v-icon>
              <span class="font-weight-medium">{{ a.name }}</span>
              <v-btn
                :icon="copiedAgentId === a.id ? 'mdi-check' : 'mdi-content-copy'"
                size="x-small"
                variant="text"
                :color="copiedAgentId === a.id ? 'success' : undefined"
                class="ml-1"
                @click="copyAgentId(a.id)"
                :aria-label="`Copy agent ID for ${a.name}`"
                :title="copiedAgentId === a.id ? 'Copied!' : 'Copy agent ID'"
              />
              <v-spacer />
              <v-chip size="x-small" :color="statusColor(a)" variant="flat">
                {{ a.is_online ? 'Online' : a.status }}
              </v-chip>
            </div>
            <div class="text-caption text-medium-emphasis mb-2">
              <span :title="`Agent ID: ${a.id}`">id: {{ shortId(a.id) }}</span>
              <span class="mx-1">·</span>
              <span :title="`machine_id: ${a.machine_id}`">{{ shortId(a.machine_id) }}</span>
            </div>
            <div class="d-flex flex-wrap gap-1 mb-2">
              <v-chip
                size="x-small"
                :prepend-icon="osIcon(a.os)"
                variant="tonal"
              >
                {{ a.os }}
              </v-chip>
              <v-chip
                v-if="a.agent_version"
                size="x-small"
                variant="tonal"
              >
                v{{ a.agent_version }}
              </v-chip>
              <v-chip
                v-for="codec in codecChips(a)"
                :key="codec.label"
                size="x-small"
                :color="codec.color"
                variant="tonal"
                :title="codec.tooltip"
              >
                {{ codec.label }}
              </v-chip>
            </div>
            <div class="text-caption text-medium-emphasis mb-2">
              Last seen: {{ fmtDate(a.last_seen_at) }}
            </div>
            <div class="d-flex gap-2">
              <v-btn
                size="small"
                variant="tonal"
                color="primary"
                prepend-icon="mdi-remote-desktop"
                :disabled="!a.is_online"
                :to="{ name: 'agent-remote', params: { tenantId, agentId: a.id } }"
                :aria-label="`Connect to agent ${a.name}`"
                class="flex-grow-1"
              >
                Connect
              </v-btn>
              <v-btn
                icon="mdi-alert-circle-outline"
                size="small"
                variant="text"
                color="warning"
                @click="openCrashes(a)"
                :aria-label="`View crash reports for ${a.name}`"
              />
              <v-btn
                icon="mdi-text-box-search-outline"
                size="small"
                variant="text"
                color="primary"
                @click="openLogs(a)"
                :aria-label="`View logs for ${a.name}`"
              />
              <v-btn
                icon="mdi-delete"
                size="small"
                variant="text"
                color="error"
                @click="confirmDelete(a)"
                :aria-label="`Delete agent ${a.name}`"
              />
            </div>
          </v-card-text>
        </v-card>
      </v-list>

      <div v-else class="text-center pa-4 pa-md-6 pa-lg-8 text-medium-emphasis">
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

  <!-- Crash reports modal -->
  <AgentCrashesDialog
    v-model="crashesDialogOpen"
    :tenant-id="tenantId"
    :agent-id="crashesTarget?.id ?? ''"
    :agent-name="crashesTarget?.name ?? ''"
  />

  <!-- Logs viewer modal (centralized agent-log upload, rc.58/rc.59) -->
  <AgentLogsDialog
    v-model="logsDialogOpen"
    :tenant-id="tenantId"
    :agent-id="logsTarget?.id ?? ''"
    :agent-name="logsTarget?.name ?? ''"
  />

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
import { useDisplay } from 'vuetify'
import { useAgentStore, type Agent, type EnrollmentToken } from '@/stores/agents'
import { codecChips } from './agentCodecChips'
import AgentCrashesDialog from './AgentCrashesDialog.vue'
import AgentLogsDialog from './AgentLogsDialog.vue'

const props = defineProps<{ tenantId: string }>()

const agentStore = useAgentStore()

// `mobile` flips below sm (~600px) so the table stays usable on tablets
// and small laptops; `lgAndDown` (≤1280) drives the codec-chip rollup so
// mid-width viewports don't blow past the Actions column.
const { smAndDown: mobile, lgAndDown } = useDisplay()

const enrollDialogOpen = ref(false)
const enrollLoading = ref(false)
const enrollToken = ref<EnrollmentToken | null>(null)
const enrollError = ref<string | null>(null)
const copied = ref(false)

// Per-row copy-feedback for the agent-id copy button. Holds the id
// of the last-copied agent for 2 s so the row's mdi-content-copy
// icon swaps to mdi-check (and back) without us having to thread
// state through each row.
const copiedAgentId = ref<string | null>(null)
let copiedAgentIdTimer: ReturnType<typeof setTimeout> | null = null

const deleteDialogOpen = ref(false)
const deleteTarget = ref<Agent | null>(null)
const deleting = ref(false)

// Crash-reports modal state (Task 9 Phase 3). Re-fetches on open via
// the dialog's own watcher; no caching here.
const crashesDialogOpen = ref(false)
const crashesTarget = ref<Agent | null>(null)

function openCrashes(a: Agent) {
  crashesTarget.value = a
  crashesDialogOpen.value = true
}

// Logs-viewer modal state (rc.74). Same no-cache, fetch-on-open
// pattern as the crashes dialog — the AgentLogsDialog refetches the
// recent uploaded log batches each time it opens.
const logsDialogOpen = ref(false)
const logsTarget = ref<Agent | null>(null)

function openLogs(a: Agent) {
  logsTarget.value = a
  logsDialogOpen.value = true
}

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

function fmtRelative(iso: string): string {
  if (!iso) return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const diff = Date.now() - d.getTime()
  if (diff < 0) return 'just now'
  const mins = Math.floor(diff / 60000)
  if (mins < 1) return 'just now'
  if (mins < 60) return `${mins}m ago`
  const hours = Math.floor(mins / 60)
  if (hours < 24) return `${hours}h ago`
  const days = Math.floor(hours / 24)
  if (days < 30) return `${days}d ago`
  const months = Math.floor(days / 30)
  if (months < 12) return `${months}mo ago`
  return `${Math.floor(months / 12)}y ago`
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

/**
 * Copy an agent's hex ObjectId to the clipboard so the operator can
 * paste it into `roomler-tunnel forward --agent <hex>` (or any other
 * CLI / API call that needs the agent identifier).
 *
 * Surfaces transient success state via [`copiedAgentId`] for 2 s —
 * the row's copy button swaps to mdi-check during that window.
 *
 * Best-effort: a clipboard-write failure is logged but otherwise
 * silently swallowed; the operator can fall back to copying from
 * the tooltip (the full id is rendered in the row's `title`
 * attribute, so a long-hover surfaces it natively).
 */
async function copyAgentId(id: string) {
  try {
    await navigator.clipboard.writeText(id)
    copiedAgentId.value = id
    if (copiedAgentIdTimer !== null) {
      clearTimeout(copiedAgentIdTimer)
    }
    copiedAgentIdTimer = setTimeout(() => {
      copiedAgentId.value = null
      copiedAgentIdTimer = null
    }, 2000)
  } catch (err) {
    console.warn('copyAgentId: clipboard write failed', err)
  }
}

/**
 * Truncate a hex id to a 6-char prefix + ellipsis for inline display.
 * The full id is always available via the cell's `title` attribute
 * (long-press / hover tooltip) and the Copy button puts the full
 * value on the clipboard. Inline truncation keeps the Agents table
 * from blowing past the Actions column on mid-width viewports.
 */
function shortId(id: string): string {
  if (!id) return '—'
  if (id.length <= 8) return id
  return `${id.slice(0, 6)}…`
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

<style scoped>
/* Action column: keep it leftmost AND narrow so mid-width viewports never
   push it off-screen. Two icon buttons fit in ~96px. */
.agents-table :deep(th.agents-actions-col),
.agents-table :deep(td.agents-actions-col) {
  width: 96px;
  min-width: 96px;
  white-space: nowrap;
}
</style>
