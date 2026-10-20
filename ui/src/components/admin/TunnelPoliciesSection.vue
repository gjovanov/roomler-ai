<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-card>
    <v-card-title class="d-flex align-center flex-wrap ga-2">
      <span>Tunnel ACL</span>
      <v-spacer />
      <v-btn
        prepend-icon="mdi-plus"
        color="primary"
        variant="flat"
        size="small"
        @click="openCreateDialog"
      >
        New policy
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

      <p v-if="!store.loading && store.policies.length === 0" class="text-medium-emphasis">
        No tunnel policies defined yet. Server-side ACL is default-deny — until
        you add at least one policy, every <code>roomler forward</code>
        request will be rejected with <code>acl_denied</code>.
      </p>

      <div
        v-if="store.loading && store.policies.length === 0"
        class="d-flex justify-center pa-8"
      >
        <v-progress-circular indeterminate />
      </div>

      <!-- A card per policy below 600 px (table-stack-xs): the allowlist,
           the ceilings and the actions were off-screen on a phone. -->
      <v-table v-else-if="store.policies.length > 0" density="compact" class="table-stack-xs">
        <thead>
          <tr>
            <th>Name</th>
            <th>Subjects</th>
            <th>Targets</th>
            <th>Allowlist</th>
            <th>Ceilings</th>
            <th class="text-right">Actions</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="p in store.policies" :key="p.id">
            <td data-label="Name">
              <div class="font-weight-medium">{{ p.name }}</div>
              <div class="text-caption text-medium-emphasis">
                {{ formatDate(p.created_at) }}
              </div>
            </td>
            <td data-label="Subjects">
              <v-chip
                v-for="(s, i) in p.subjects"
                :key="i"
                size="x-small"
                variant="tonal"
                class="mr-1 mb-1"
              >
                {{ subjectLabel(s) }}
              </v-chip>
            </td>
            <td data-label="Targets">
              <v-chip
                v-for="(t, i) in p.targets"
                :key="i"
                size="x-small"
                variant="tonal"
                class="mr-1 mb-1"
              >
                {{ targetLabel(t) }}
              </v-chip>
            </td>
            <td data-label="Allowlist">
              <div v-for="(r, i) in p.allowlist" :key="i" class="text-caption">
                <code class="text-break">{{ hostLabel(r.host_pattern) }}:{{ portRangeLabel(r.port_range) }}</code>
                <span class="text-medium-emphasis">/{{ (r.proto ?? 'any') }}</span>
              </div>
            </td>
            <td class="text-caption" data-label="Ceilings">
              <div>
                flows:
                <strong>{{ p.max_concurrent_flows ?? '∞' }}</strong>
              </div>
              <div>
                bytes:
                <strong>{{ p.max_bytes_per_session ? formatBytes(p.max_bytes_per_session) : '∞' }}</strong>
              </div>
            </td>
            <td class="text-right">
              <v-btn
                icon="mdi-pencil"
                size="small"
                variant="text"
                @click="openEditDialog(p)"
              />
              <v-btn
                icon="mdi-delete"
                size="small"
                variant="text"
                color="error"
                @click="confirmDelete(p)"
              />
            </td>
          </tr>
        </tbody>
      </v-table>
    </v-card-text>

    <!-- Create/edit dialog ──────────────────────────────────────── -->
    <v-dialog v-model="editDialog" max-width="900" persistent scrollable :fullscreen="fullscreen">
      <v-card>
        <v-card-title>
          {{ editingId ? 'Edit tunnel policy' : 'New tunnel policy' }}
        </v-card-title>
        <v-card-text>
          <v-alert
            v-if="formError"
            type="error"
            variant="tonal"
            closable
            @click:close="formError = null"
            class="mb-4"
          >
            {{ formError }}
          </v-alert>

          <v-text-field
            v-model="form.name"
            label="Name"
            placeholder="e.g. ops-team-prod-postgres"
            variant="outlined"
            density="compact"
            class="mb-4"
            :error-messages="nameError"
          />

          <!-- Subjects -->
          <div class="d-flex align-center flex-wrap ga-1 mb-2">
            <strong>Subjects</strong>
            <span class="text-medium-emphasis text-caption ml-2">
              Who this policy applies to. First-match-wins across all policies.
            </span>
            <v-spacer />
            <v-btn
              size="x-small"
              variant="tonal"
              prepend-icon="mdi-plus"
              @click="addSubject"
            >
              Add subject
            </v-btn>
          </div>
          <div
            v-for="(s, i) in form.subjects"
            :key="`s-${i}`"
            class="d-flex align-center flex-wrap mb-2 ga-2"
          >
            <v-select
              :model-value="s.kind"
              :items="subjectKinds"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 150px; max-width: 220px"
              @update:model-value="setSubjectKind(i, $event)"
            />
            <!-- Pick from the fleet where a store already has the rows
                 (agents / tunnel clients) — ids were hand-typed before. -->
            <v-select
              v-if="s.kind === 'agent_id'"
              v-model="s.id"
              :items="withCurrent(agentItems, s.id)"
              :label="subjectIdLabel(s.kind)"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 200px"
            />
            <v-select
              v-else-if="s.kind === 'tunnel_client_id'"
              v-model="s.id"
              :items="withCurrent(clientItems, s.id)"
              :label="subjectIdLabel(s.kind)"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 200px"
            />
            <!-- Roles and people are picked, as on the Overlay tab: a 24-hex
                 ObjectId typed into a field was the only way before, and on a
                 phone it was no way at all. -->
            <v-select
              v-else-if="s.kind === 'role_id'"
              v-model="s.id"
              :items="withCurrent(roleItems, s.id)"
              :label="subjectIdLabel(s.kind)"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 200px"
            />
            <v-select
              v-else-if="s.kind === 'user_id'"
              v-model="s.id"
              :items="withCurrent(memberItems, s.id)"
              :label="subjectIdLabel(s.kind)"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 200px"
            />
            <span v-else class="text-medium-emphasis text-caption">
              (catch-all — matches every user in the tenant)
            </span>
            <v-btn
              icon="mdi-close"
              size="x-small"
              variant="text"
              @click="removeSubject(i)"
            />
          </div>

          <v-divider class="my-4" />

          <!-- Targets -->
          <div class="d-flex align-center flex-wrap ga-1 mb-2">
            <strong>Targets</strong>
            <span class="text-medium-emphasis text-caption ml-2">
              Which agents this policy lets the subjects reach.
            </span>
            <v-spacer />
            <v-btn
              size="x-small"
              variant="tonal"
              prepend-icon="mdi-plus"
              @click="addTarget"
            >
              Add target
            </v-btn>
          </div>
          <div
            v-for="(t, i) in form.targets"
            :key="`t-${i}`"
            class="d-flex align-center flex-wrap mb-2 ga-2"
          >
            <v-select
              :model-value="t.kind"
              :items="targetKinds"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 150px; max-width: 220px"
              @update:model-value="setTargetKind(i, $event)"
            />
            <v-select
              v-if="t.kind === 'agent_id'"
              v-model="t.id"
              :items="withCurrent(agentItems, t.id)"
              label="Agent"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 200px"
            />
            <span v-else class="text-medium-emphasis text-caption">
              (catch-all — matches every agent in the tenant)
            </span>
            <v-btn
              icon="mdi-close"
              size="x-small"
              variant="text"
              @click="removeTarget(i)"
            />
          </div>

          <v-divider class="my-4" />

          <!-- Allowlist -->
          <div class="d-flex align-center flex-wrap ga-1 mb-2">
            <strong>Destination allowlist</strong>
            <span class="text-medium-emphasis text-caption ml-2">
              Host + port-range patterns the subjects may reach via the targets.
            </span>
            <v-spacer />
            <v-btn
              size="x-small"
              variant="tonal"
              prepend-icon="mdi-plus"
              @click="addAllowRule"
            >
              Add rule
            </v-btn>
          </div>
          <div
            v-for="(r, i) in form.allowlist"
            :key="`r-${i}`"
            class="d-flex align-center flex-wrap mb-2 ga-2"
          >
            <v-select
              :model-value="r.host_pattern.kind"
              :items="hostKinds"
              label="Match"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 130px; max-width: 160px"
              @update:model-value="setHostKind(i, $event)"
            />
            <v-text-field
              v-model="r.host_pattern.value"
              :label="hostValueLabel(r.host_pattern.kind)"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 220px"
            />
            <v-text-field
              v-model.number="r.port_range.low"
              label="Port (low)"
              type="number"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 96px; max-width: 120px"
            />
            <v-text-field
              v-model.number="r.port_range.high"
              label="Port (high)"
              type="number"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 96px; max-width: 120px"
            />
            <v-select
              v-model="r.proto"
              :items="protoKinds"
              label="Proto"
              variant="outlined"
              density="compact"
              hide-details
              style="flex: 1 1 96px; max-width: 120px"
            />
            <v-btn
              icon="mdi-close"
              size="x-small"
              variant="text"
              @click="removeAllowRule(i)"
            />
          </div>

          <v-divider class="my-4" />

          <!-- Ceilings -->
          <div class="d-flex align-center flex-wrap ga-1 mb-2">
            <strong>Per-session ceilings</strong>
            <span class="text-medium-emphasis text-caption ml-2">
              Leave blank for unlimited.
            </span>
          </div>
          <div class="d-flex flex-wrap ga-3">
            <v-text-field
              v-model.number="form.max_concurrent_flows"
              label="Max concurrent flows"
              type="number"
              variant="outlined"
              density="compact"
              hide-details
              clearable
              placeholder="e.g. 64"
              style="flex: 1 1 200px; max-width: 240px"
            />
            <v-text-field
              v-model.number="form.max_bytes_per_session"
              label="Max bytes / session"
              type="number"
              variant="outlined"
              density="compact"
              hide-details
              clearable
              placeholder="e.g. 10737418240 (10 GiB)"
              style="flex: 1 1 200px; max-width: 280px"
            />
          </div>
        </v-card-text>

        <v-card-actions>
          <v-spacer />
          <v-btn @click="cancelEdit">Cancel</v-btn>
          <v-btn color="primary" :loading="saving" @click="savePolicy">
            {{ editingId ? 'Save changes' : 'Create policy' }}
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>

    <!-- Delete confirm dialog ──────────────────────────────────── -->
    <v-dialog v-model="deleteDialog" max-width="500">
      <v-card>
        <v-card-title>Delete tunnel policy?</v-card-title>
        <v-card-text>
          This will soft-delete <strong>{{ deleteTarget?.name }}</strong>.
          Live tunnel sessions keep working; new
          <code>TcpForwardRequest</code>s relying on this policy alone will
          start being rejected with <code>acl_denied</code>.
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn @click="deleteDialog = false">Cancel</v-btn>
          <v-btn color="error" :loading="deleting" @click="doDelete">
            Delete
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </v-card>
</template>

<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import {
  type DestinationRule,
  type HostPattern,
  type PolicySubject,
  type PolicyTarget,
  type ProtocolKind,
  type TunnelPolicy,
  type TunnelPolicyCreate,
  useTunnelPolicyStore,
} from '@/stores/tunnelPolicies'
import { useAgentStore } from '@/stores/agents'
import { useTunnelClientStore } from '@/stores/tunnelClients'
import { useRoleStore } from '@/stores/role'
import { useMembersStore, type Member } from '@/stores/members'
import { useNodeLabel } from '@/composables/useNodeLabel'
import { useResponsiveDialog } from '@/composables/useResponsiveDialog'
import { deviceLabel } from '@/composables/useHideDeviceName'

const props = defineProps<{ tenantId: string }>()
const store = useTunnelPolicyStore()
// Fleet stores back the agent / tunnel-client PICKERS in the editor —
// subjects/targets were hand-typed 24-hex ids before (2026-08-04).
const agentStore = useAgentStore()
const tunnelClientStore = useTunnelClientStore()
const roleStore = useRoleStore()
const membersStore = useMembersStore()
const { agentLabel, clientLabel } = useNodeLabel()
const { fullscreen } = useResponsiveDialog()

// The pickers name devices as the Devices page does (FR-116), and roles and
// people by name — the subject list used to show `Agent 64cd34…`.
const agentItems = computed(() =>
  agentStore.agents.map((a) => ({ title: deviceLabel(a), value: a.id })),
)
const clientItems = computed(() =>
  tunnelClientStore.clients.map((c) => ({ title: deviceLabel(c), value: c.id })),
)
const roleItems = computed(() => roleStore.roles.map((r) => ({ title: r.name, value: r.id })))
// Page 1 of the members (25), as the Overlay tab's picker: a policy that
// names someone beyond it still shows them, by id, through withCurrent().
const memberItems = computed(() =>
  membersStore.items.map((m: Member) => ({
    title: m.display_name || m.nickname || m.user_id,
    value: m.user_id,
  })),
)

/** The picker's items, plus the value a stored policy already holds when the
 *  list does not (a device beyond the first page, someone not in page 1 of
 *  the members) — so editing a policy never blanks a subject it had. */
function withCurrent(items: { title: string; value: string }[], current: string) {
  if (!current || items.some((i) => i.value === current)) return items
  return [...items, { title: `${current.slice(0, 6)}…`, value: current }]
}

const subjectKinds = [
  { title: 'All users', value: 'all_users' },
  { title: 'User', value: 'user_id' },
  { title: 'Role', value: 'role_id' },
  { title: 'Tunnel client', value: 'tunnel_client_id' },
  // 2026-08-04 — the wire supported AgentId subjects (node-stack
  // unification P3b-2: an agent acting as a tunnel ORIGIN) but the picker
  // never offered it.
  { title: 'Agent', value: 'agent_id' },
]
const targetKinds = [
  { title: 'All agents', value: 'all_agents' },
  { title: 'Agent', value: 'agent_id' },
]
const hostKinds = [
  { title: 'Exact', value: 'exact' },
  { title: 'Wildcard', value: 'wildcard' },
  { title: 'CIDR', value: 'cidr' },
]
const protoKinds = [
  { title: 'Any', value: 'any' },
  { title: 'TCP', value: 'tcp' },
  { title: 'UDP', value: 'udp' },
]

const editDialog = ref(false)
const editingId = ref<string | null>(null)
const saving = ref(false)
const formError = ref<string | null>(null)

interface FormSubject {
  kind: 'all_users' | 'user_id' | 'role_id' | 'tunnel_client_id' | 'agent_id'
  id: string
}
interface FormTarget {
  kind: 'all_agents' | 'agent_id'
  id: string
}
interface FormRule {
  host_pattern: { kind: 'exact' | 'wildcard' | 'cidr'; value: string }
  port_range: { low: number; high: number }
  proto: ProtocolKind
}

const form = reactive<{
  name: string
  subjects: FormSubject[]
  targets: FormTarget[]
  allowlist: FormRule[]
  max_concurrent_flows: number | null
  max_bytes_per_session: number | null
}>(emptyForm())

function emptyForm() {
  return {
    name: '',
    subjects: [{ kind: 'all_users' as const, id: '' }],
    targets: [{ kind: 'all_agents' as const, id: '' }],
    allowlist: [
      {
        host_pattern: { kind: 'exact' as const, value: '' },
        port_range: { low: 1, high: 65535 },
        proto: 'any' as ProtocolKind,
      },
    ],
    max_concurrent_flows: null as number | null,
    max_bytes_per_session: null as number | null,
  }
}

const nameError = computed(() => {
  if (!form.name.trim()) return 'Required.'
  return ''
})

function openCreateDialog() {
  editingId.value = null
  formError.value = null
  Object.assign(form, emptyForm())
  editDialog.value = true
}

function openEditDialog(p: TunnelPolicy) {
  editingId.value = p.id
  formError.value = null
  form.name = p.name
  form.subjects = p.subjects.map((s) =>
    s.kind === 'all_users' ? { kind: 'all_users', id: '' } : { kind: s.kind, id: s.id },
  )
  form.targets = p.targets.map((t) =>
    t.kind === 'all_agents' ? { kind: 'all_agents', id: '' } : { kind: 'agent_id', id: t.id },
  )
  form.allowlist = p.allowlist.map((r) => ({
    host_pattern: { kind: r.host_pattern.kind, value: r.host_pattern.value },
    port_range: { low: r.port_range.low, high: r.port_range.high },
    proto: r.proto ?? 'any',
  }))
  form.max_concurrent_flows = p.max_concurrent_flows
  form.max_bytes_per_session = p.max_bytes_per_session
  editDialog.value = true
}

function addSubject() {
  form.subjects.push({ kind: 'user_id', id: '' })
}
function removeSubject(i: number) {
  form.subjects.splice(i, 1)
}
function setSubjectKind(i: number, kind: string) {
  form.subjects[i] = { kind: kind as FormSubject['kind'], id: '' }
}
function subjectIdLabel(kind: string) {
  switch (kind) {
    case 'user_id': return 'User'
    case 'role_id': return 'Role'
    case 'tunnel_client_id': return 'Tunnel client'
    case 'agent_id': return 'Agent'
    default: return 'ID'
  }
}

function addTarget() {
  form.targets.push({ kind: 'agent_id', id: '' })
}
function removeTarget(i: number) {
  form.targets.splice(i, 1)
}
function setTargetKind(i: number, kind: string) {
  form.targets[i] = { kind: kind as FormTarget['kind'], id: '' }
}

function addAllowRule() {
  form.allowlist.push({
    host_pattern: { kind: 'exact', value: '' },
    port_range: { low: 1, high: 65535 },
    proto: 'any',
  })
}
function removeAllowRule(i: number) {
  form.allowlist.splice(i, 1)
}
function setHostKind(i: number, kind: string) {
  form.allowlist[i].host_pattern.kind = kind as FormRule['host_pattern']['kind']
}
function hostValueLabel(kind: string) {
  switch (kind) {
    case 'exact': return 'Hostname (e.g. db.intranet)'
    case 'wildcard': return 'Wildcard (e.g. *.intranet)'
    case 'cidr': return 'CIDR (e.g. 10.0.0.0/24)'
    default: return 'Value'
  }
}

function buildPayload(): TunnelPolicyCreate {
  return {
    name: form.name.trim(),
    subjects: form.subjects.map((s) =>
      s.kind === 'all_users' ? { kind: 'all_users' } as PolicySubject : { kind: s.kind, id: s.id.trim() } as PolicySubject,
    ),
    targets: form.targets.map((t) =>
      t.kind === 'all_agents' ? { kind: 'all_agents' } as PolicyTarget : { kind: 'agent_id', id: t.id.trim() } as PolicyTarget,
    ),
    allowlist: form.allowlist.map((r) => ({
      host_pattern: { kind: r.host_pattern.kind, value: r.host_pattern.value.trim() } as HostPattern,
      port_range: { low: Number(r.port_range.low), high: Number(r.port_range.high) },
      proto: r.proto,
    })) as DestinationRule[],
    max_concurrent_flows: form.max_concurrent_flows ?? null,
    max_bytes_per_session: form.max_bytes_per_session ?? null,
  }
}

function validateForm(): string | null {
  if (!form.name.trim()) return 'Name is required.'
  if (form.subjects.length === 0) return 'At least one subject is required.'
  if (form.targets.length === 0) return 'At least one target is required.'
  if (form.allowlist.length === 0) return 'At least one allowlist rule is required.'
  for (const s of form.subjects) {
    if (s.kind !== 'all_users' && !s.id.trim()) {
      return 'Each non-AllUsers subject needs an ID.'
    }
  }
  for (const t of form.targets) {
    if (t.kind === 'agent_id' && !t.id.trim()) {
      return 'Agent-ID targets need an Agent ID.'
    }
  }
  for (const r of form.allowlist) {
    if (!r.host_pattern.value.trim()) return 'Each allowlist rule needs a host value.'
    if (!r.port_range.low || !r.port_range.high) return 'Port range must be set.'
    if (r.port_range.low < 1 || r.port_range.low > 65535) return 'Port-range low must be 1-65535.'
    if (r.port_range.high < r.port_range.low) return 'Port-range high must be ≥ low.'
    if (r.port_range.high > 65535) return 'Port-range high must be ≤ 65535.'
  }
  return null
}

async function savePolicy() {
  const err = validateForm()
  if (err) {
    formError.value = err
    return
  }
  saving.value = true
  formError.value = null
  try {
    const payload = buildPayload()
    if (editingId.value) {
      await store.updatePolicy(props.tenantId, editingId.value, payload)
    } else {
      await store.createPolicy(props.tenantId, payload)
    }
    editDialog.value = false
  } catch (e) {
    formError.value = (e as Error).message
  } finally {
    saving.value = false
  }
}

function cancelEdit() {
  editDialog.value = false
}

const deleteDialog = ref(false)
const deleteTarget = ref<TunnelPolicy | null>(null)
const deleting = ref(false)

function confirmDelete(p: TunnelPolicy) {
  deleteTarget.value = p
  deleteDialog.value = true
}

async function doDelete() {
  if (!deleteTarget.value) return
  deleting.value = true
  try {
    await store.deletePolicy(props.tenantId, deleteTarget.value.id)
    deleteDialog.value = false
    deleteTarget.value = null
  } catch (e) {
    store.error = (e as Error).message
  } finally {
    deleting.value = false
  }
}

/** A subject by name — the id prefix only when the name is not known here. */
function subjectLabel(s: PolicySubject): string {
  if (s.kind === 'all_users') return 'All users'
  const id = s.id.length > 8 ? `${s.id.slice(0, 6)}…` : s.id
  switch (s.kind) {
    case 'user_id': return `User ${memberItems.value.find((m) => m.value === s.id)?.title ?? id}`
    case 'role_id': return `Role ${roleItems.value.find((r) => r.value === s.id)?.title ?? id}`
    case 'tunnel_client_id': return `Client ${clientLabel(s.id, id)}`
    case 'agent_id': return `Agent ${agentLabel(s.id, id)}`
  }
}

function targetLabel(t: PolicyTarget): string {
  if (t.kind === 'all_agents') return 'All agents'
  const id = t.id.length > 8 ? `${t.id.slice(0, 6)}…` : t.id
  return `Agent ${agentLabel(t.id, id)}`
}

function hostLabel(h: HostPattern): string {
  switch (h.kind) {
    case 'exact': return h.value
    case 'wildcard': return h.value
    case 'cidr': return h.value
  }
}

function portRangeLabel(r: { low: number; high: number }): string {
  return r.low === r.high ? `${r.low}` : `${r.low}-${r.high}`
}

function formatDate(iso: string): string {
  try { return new Date(iso).toLocaleString() } catch { return iso }
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MiB`
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GiB`
}

onMounted(() => {
  store.fetchPolicies(props.tenantId)
  roleStore.fetchRoles(props.tenantId)
  membersStore.fetchMembers(props.tenantId)
  if (agentStore.agents.length === 0) agentStore.fetchAgents(props.tenantId)
  if (tunnelClientStore.clients.length === 0) {
    tunnelClientStore.fetchTunnelClients(props.tenantId)
  }
})
</script>
