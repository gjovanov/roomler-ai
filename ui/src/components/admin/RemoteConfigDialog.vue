<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-dialog v-model="open" max-width="760" scrollable>
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-cog-transfer-outline" color="primary" class="mr-2" />
        <span>Device configuration — {{ agent?.name }}</span>
      </v-card-title>

      <v-card-text>
        <!-- Lead with what this screen IS. Every other dialog here writes a
             server-side policy that takes effect the moment it saves; this one
             writes a REQUEST that a device may decline, and a reader who
             assumes otherwise will misread everything below it. -->
        <v-alert
          type="info"
          variant="tonal"
          density="compact"
          class="mb-4"
          icon="mdi-transfer-right"
        >
          This records what you want the device to run. The device applies it
          the next time it connects — and it may refuse: these switches belong
          to whoever holds the machine, and a device only accepts them after
          someone enables <code>remote_config_enabled</code> on it directly.
        </v-alert>

        <!-- Where the last request actually got to. This is the whole reason
             the device reports back; without it, "saved" is all we could
             honestly say, and "saved" is not the question anyone is asking. -->
        <v-alert
          v-if="status"
          :type="status.type"
          variant="tonal"
          density="compact"
          class="mb-4"
          :icon="status.icon"
        >
          <div class="font-weight-medium">{{ status.title }}</div>
          <div class="text-body-2 mt-1">{{ status.detail }}</div>
          <div v-if="reportedKeys.live.length" class="text-body-2 mt-2">
            In force now: <strong>{{ reportedKeys.live.join(', ') }}</strong>
          </div>
          <div v-if="reportedKeys.needsRestart.length" class="text-body-2 mt-1">
            Saved on the device, waiting for a restart:
            <strong>{{ reportedKeys.needsRestart.join(', ') }}</strong>
          </div>
          <div v-if="report?.detail" class="text-body-2 mt-2 font-italic">
            {{ report.detail }}
          </div>
        </v-alert>

        <!-- ── Remote execution ──────────────────────────────────────────── -->
        <div class="text-subtitle-2 mb-1">Remote execution</div>
        <ManagedSwitch
          v-model="draft.exec_enabled"
          :disabled="!mayGrantExec"
          label="Accept remote commands (exec_enabled)"
        />
        <div class="text-caption text-medium-emphasis mb-1 ml-1">
          Commands run as the daemon — <strong>SYSTEM</strong> on Windows,
          <strong>root</strong> under systemd. Takes effect immediately on the
          device; no restart.
        </div>
        <div v-if="!mayGrantExec" class="text-caption text-error mb-4 ml-1">
          You need the <strong>EXEC_DEVICE</strong> permission to change this.
          Opening a door you cannot walk through is the same escalation as
          granting yourself the key.
        </div>
        <div v-else class="mb-4" />

        <!-- ── SSH ───────────────────────────────────────────────────────── -->
        <div class="text-subtitle-2 mb-1">SSH</div>
        <ManagedSwitch
          v-model="draft.ssh_enabled"
          :disabled="!mayGrantSsh"
          label="Run the SSH server (ssh_enabled)"
        />

        <v-expand-transition>
          <div v-if="sshDetailsShown">
            <v-textarea
              v-model="authorizedKeysText"
              :disabled="!mayGrantSsh"
              label="Authorized keys — one OpenSSH public key per line"
              placeholder="ssh-ed25519 AAAAC3Nza… you@laptop"
              rows="3"
              density="compact"
              variant="outlined"
              class="mt-3"
              hide-details
              persistent-placeholder
            />
            <div class="text-caption text-medium-emphasis mb-3 ml-1">
              Leave the box untouched to keep whatever the device already has.
              An empty list means <strong>nobody</strong>.
            </div>

            <v-select
              v-model="draft.ssh_account_mode"
              :disabled="!mayGrantSsh"
              :items="accountOptions"
              label="Key-list sessions run as"
              density="compact"
              variant="outlined"
              clearable
              hide-details
              class="mb-1"
            />
            <div class="text-caption text-medium-emphasis mb-3 ml-1">
              Cleared = not managed. Unset <em>on the device</em> means a
              key-list session authenticates and then runs
              <strong>nothing</strong> — deliberately, so an unread setting
              cannot quietly hand out SYSTEM/root.
            </div>

            <v-text-field
              v-model="sshPortText"
              :disabled="!mayGrantSsh"
              label="SSH port (blank = not managed; the device's default is 2222)"
              density="compact"
              variant="outlined"
              type="number"
              hide-details
              class="mb-1"
            />
            <div class="text-caption text-medium-emphasis mb-3 ml-1">
              2222 rather than 22 so an existing <code>sshd</code> keeps serving
              the overlay address.
            </div>
          </div>
        </v-expand-transition>

        <div v-if="!mayGrantSsh" class="text-caption text-error mb-4 ml-1">
          You need the <strong>SSH_DEVICE</strong> permission to change any of
          these. Handing out authorized keys or an account mode is granting SSH
          just as much as flipping the switch is.
        </div>

        <!-- ── Video encoders (FR-77) ───────────────────────────────────── -->
        <div class="text-subtitle-2 mb-1 mt-2">Video encoders</div>
        <v-text-field
          v-model="encoderDenyText"
          label="Encoder cells to deny (encoder_cells_deny)"
          placeholder="hevc_qsv:yuv444, hevc_vaapi:yuv444"
          density="compact"
          variant="outlined"
          hide-details
          persistent-placeholder
          class="mb-1"
        />
        <div class="text-caption text-medium-emphasis mb-4 ml-1">
          Comma-separated <code>name:chroma</code> cells the device must not
          open or advertise — the cell matrix's kill switch. Blank = not
          managed (the device keeps its own list, built-in
          <code>hevc_qsv:yuv444, hevc_vaapi:yuv444</code>);
          <code>none</code> = deny nothing. It only ever removes cells, so no
          extra permission bit; takes effect at the next daemon restart.
        </div>

        <!-- The combination checks. Each of these produces a device that is
             "on" and reaches nobody — the exact silent-nothing this feature
             exists to remove, so it must be caught here rather than discovered
             later by someone whose ssh just hangs up on them. -->
        <v-alert
          v-for="w in combinationWarnings"
          :key="w"
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-2"
          icon="mdi-alert-outline"
        >
          {{ w }}
        </v-alert>

        <!-- The gates this dialog cannot reach, in the order the device
             evaluates them. Without these an admin sets everything here and is
             left wondering why nothing happened. -->
        <v-alert
          v-if="orgExecDisabled && draft.exec_enabled === true"
          type="info"
          variant="tonal"
          density="compact"
          class="mb-2"
        >
          Remote execution is switched off for the whole organization, so this
          device will still refuse. An org owner enables it in Settings.
        </v-alert>
        <v-alert
          v-if="orgSshDisabled && draft.ssh_enabled === true"
          type="info"
          variant="tonal"
          density="compact"
          class="mb-2"
        >
          SSH is switched off for the whole organization — a separate switch
          from remote execution. An org owner enables it in Settings.
        </v-alert>

        <v-alert
          v-if="error"
          type="error"
          variant="tonal"
          density="compact"
          class="mt-2"
        >
          {{ error }}
        </v-alert>
      </v-card-text>

      <v-card-actions>
        <v-btn
          v-if="hasRequest"
          variant="text"
          color="error"
          :loading="clearing"
          @click="clearAll"
        >
          Stop managing
        </v-btn>
        <v-spacer />
        <v-btn variant="text" @click="close">Cancel</v-btn>
        <v-btn color="primary" :loading="saving" :disabled="!dirty" @click="save">
          Request
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useAgentStore, type Agent, type DesiredConfig } from '@/stores/agents'
import { useTenantStore } from '@/stores/tenant'
import { canGrantDeviceExec, canGrantDeviceSsh } from '@/utils/permissions'
import ManagedSwitch from './ManagedSwitch.vue'

const props = defineProps<{
  modelValue: boolean
  tenantId: string
  agent: Agent
}>()

const emit = defineEmits<{
  (e: 'update:modelValue', v: boolean): void
  (e: 'saved'): void
}>()

const open = computed({
  get: () => props.modelValue,
  set: (v) => emit('update:modelValue', v),
})

const agentStore = useAgentStore()
const tenantStore = useTenantStore()
const saving = ref(false)
const clearing = ref(false)
const error = ref('')
const sshDetailsTouched = ref(false)
const agent = computed(() => props.agent)

/** Only the managed keys. `undefined` is "leave the device alone", which is
 *  why every field is optional rather than defaulted — a struct of plain
 *  values could not say "don't touch the rest", so toggling exec would
 *  silently assert a value for every other key on the surface. */
type Draft = Omit<DesiredConfig, 'revision' | 'updated_by' | 'updated_at'>

const draft = ref<Draft>({})
const original = ref('')

/** You cannot grant a permission you do not hold (#600/#605). The server
 *  enforces this in `remote_config::decide`; mirroring it here only avoids
 *  offering a control whose save would 403. */
const mayGrantExec = computed(() =>
  canGrantDeviceExec(tenantStore.myPermissions, tenantStore.isOwner),
)
const mayGrantSsh = computed(() =>
  canGrantDeviceSsh(tenantStore.myPermissions, tenantStore.isOwner),
)

/** Show the SSH details whenever ANY ssh key is under management — not just
 *  when `ssh_enabled` is.
 *
 *  Keying this on `ssh_enabled` alone hid keys that were being pushed: a device
 *  managed as "authorized keys, but leave the switch alone" opened with the
 *  section collapsed, so an operator could neither see nor edit them — and the
 *  draft still carried them, so saving silently re-asserted a list they had
 *  never been shown. */
const sshDetailsShown = computed(
  () =>
    sshDetailsTouched.value ||
    draft.value.ssh_enabled !== undefined ||
    draft.value.ssh_authorized_keys !== undefined ||
    draft.value.ssh_account_mode !== undefined ||
    draft.value.ssh_port !== undefined,
)

const orgExecDisabled = computed(() => agentStore.orgExecEnabled === false)
const orgSshDisabled = computed(() => agentStore.orgSshEnabled === false)

const remoteConfig = computed(() => agent.value?.remote_config)
const report = computed(() => remoteConfig.value?.report)
const hasRequest = computed(() => !!remoteConfig.value)

/** Only the keys the device confirmed for the CURRENT revision. A report about
 *  an older revision describes a different request, and showing its key lists
 *  next to this one's switches would attribute them to the wrong change. */
const reportedKeys = computed(() => {
  const r = report.value
  const fresh = r && r.revision === remoteConfig.value?.desired.revision
  return {
    live: fresh ? r.live : [],
    needsRestart: fresh ? r.needs_restart : [],
  }
})

/** One line per state, each naming the ACTION the reader can take. "It didn't
 *  work" is not a status; "set this key on the host" is. */
const status = computed<{ type: 'success' | 'info' | 'warning' | 'error'; icon: string; title: string; detail: string } | null>(() => {
  const rc = remoteConfig.value
  if (!rc) return null
  switch (rc.state) {
    case 'applied':
      return {
        type: 'success',
        icon: 'mdi-check-circle-outline',
        title: 'Applied',
        detail: 'The device confirmed this configuration and it is in force.',
      }
    case 'needs_restart':
      return {
        type: 'warning',
        icon: 'mdi-restart-alert',
        title: 'Saved on the device — restart required',
        detail:
          'These keys are written to disk but not yet in effect. The SSH server is spliced into the packet path when the daemon starts, so it will not serve sessions until then.',
      }
    case 'refused':
      return {
        type: 'warning',
        icon: 'mdi-hand-back-left-outline',
        title:
          report.value?.outcome === 'not_primary'
            ? 'Refused — this organization is a secondary on that host'
            : 'Refused — the device has not opted in',
        detail:
          report.value?.outcome === 'not_primary'
            ? 'These keys are machine-wide, so only the device’s primary enrollment may change them. Ask an admin of that organization, or configure the host directly.'
            : 'Someone with access to the machine must run `roomler config set remote_config_enabled true` on it. That switch cannot be set from here — which is the point: it is what keeps a compromised server from opening your devices.',
      }
    case 'failed':
      return {
        type: 'error',
        icon: 'mdi-alert-circle-outline',
        title: 'The device tried and failed',
        detail: 'It could not write its configuration. Usually a permissions or disk problem on the host.',
      }
    case 'pending':
      return {
        type: 'info',
        icon: 'mdi-clock-outline',
        title: 'Waiting for the device',
        detail:
          'Requested. The device reconciles when it next connects — an offline device picks this up on its own, nothing needs re-sending.',
      }
    case 'reports_unsupported':
      return {
        type: 'info',
        icon: 'mdi-help-circle-outline',
        title: 'This device cannot report back',
        detail: `Agent ${agent.value?.agent_version ?? '?'} understands a pushed configuration but not how to say what it did with it. It may well have applied this — there is no way to tell from here. Update the device to find out.`,
      }
    case 'push_unsupported':
      return {
        type: 'warning',
        icon: 'mdi-update',
        title: 'This device is too old',
        detail: `Agent ${agent.value?.agent_version ?? '?'} predates remote configuration, so nothing is sent to it at all. Update it, or set these keys on the host directly.`,
      }
    default:
      return null
  }
})

/** Combinations that produce a device which is "on" and reaches nobody. */
const combinationWarnings = computed<string[]>(() => {
  const out: string[] = []
  const d = draft.value
  if (d.ssh_enabled === true) {
    const keys = d.ssh_authorized_keys
    if (keys && keys.length === 0) {
      out.push(
        'SSH is on with an empty key list, which means nobody can connect. Add a key, or leave the key list unmanaged to keep the device’s own.',
      )
    }
    if (keys === undefined && !d.ssh_account_mode) {
      out.push(
        'Turning SSH on grants nothing by itself: the device also needs authorized keys and an account mode. If it already has them, ignore this.',
      )
    } else if (keys && keys.length > 0 && !d.ssh_account_mode) {
      out.push(
        'You are sending keys but not an account mode. A key-list session on a device whose account mode is unset authenticates and then runs nothing.',
      )
    }
  }
  return out
})

const authorizedKeysText = computed({
  get: () => (draft.value.ssh_authorized_keys ?? []).join('\n'),
  set: (v: string) => {
    sshDetailsTouched.value = true
    // A blank box is an EMPTY LIST, not "unmanaged" — the user typed into it.
    // Unmanaged is expressed by never touching it, which is why the getter
    // renders `undefined` as '' and this setter only ever runs on input.
    draft.value.ssh_authorized_keys = v
      .split('\n')
      .map((l) => l.trim())
      .filter((l) => l.length > 0)
  },
})

const sshPortText = computed({
  get: () => (draft.value.ssh_port === undefined ? '' : String(draft.value.ssh_port)),
  set: (v: string) => {
    sshDetailsTouched.value = true
    const n = Number.parseInt(v, 10)
    draft.value.ssh_port = v.trim() === '' || Number.isNaN(n) ? undefined : n
  },
})


/** FR-77 — the cell denylist. A blank box is "not managed" (undefined), so
 *  clearing it hands the key back to the device; the word `none` is how an
 *  operator says "deny nothing" (a config key cannot carry an empty list). */
const encoderDenyText = computed({
  get: () => draft.value.encoder_cells_deny ?? '',
  set: (v: string) => {
    const t = v.trim()
    draft.value.encoder_cells_deny = t === '' ? undefined : t
  },
})
const accountOptions = [
  { title: 'The signed-in user at the device', value: 'console_user' },
  { title: 'The daemon account (SYSTEM / root)', value: 'daemon' },
]

const dirty = computed(() => JSON.stringify(draft.value) !== original.value)

watch(
  () => props.modelValue,
  (v) => {
    if (!v) return
    error.value = ''
    sshDetailsTouched.value = false
    const d = remoteConfig.value?.desired
    draft.value = {
      exec_enabled: d?.exec_enabled,
      ssh_enabled: d?.ssh_enabled,
      ssh_authorized_keys: d?.ssh_authorized_keys,
      ssh_account_mode: d?.ssh_account_mode,
      ssh_port: d?.ssh_port,
      encoder_cells_deny: d?.encoder_cells_deny,
    }
    original.value = JSON.stringify(draft.value)
    if (agentStore.orgExecEnabled === null) void agentStore.fetchOrgExecEnabled(props.tenantId)
    if (agentStore.orgSshEnabled === null) void agentStore.fetchOrgSshEnabled(props.tenantId)
  },
)

async function submit(body: Draft) {
  error.value = ''
  try {
    await agentStore.updateDesiredConfig(props.tenantId, agent.value.id, body)
    emit('saved')
    open.value = false
  } catch (e) {
    // A 403 here is the "you cannot grant what you do not hold" rule, and the
    // dialog stays OPEN so the admin sees which bit they are missing rather
    // than walking away believing the change landed.
    error.value = e instanceof Error ? e.message : 'Could not record the request.'
  }
}

async function save() {
  saving.value = true
  try {
    await submit(draft.value)
  } finally {
    saving.value = false
  }
}

/** Stop managing every key — an empty request, which the device reads as
 *  "nothing to reconcile". Deliberately NOT the same as switching everything
 *  off: this leaves the device with whatever it has, which is what an operator
 *  handing a machine back to its owner actually wants. */
async function clearAll() {
  clearing.value = true
  try {
    await submit({})
  } finally {
    clearing.value = false
  }
}

function close() {
  open.value = false
}
</script>
