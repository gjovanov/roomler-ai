<template>
  <v-dialog v-model="open" max-width="720" scrollable>
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-console-network-outline" color="primary" class="mr-2" />
        <span>SSH policy — {{ agent?.name }}</span>
      </v-card-title>

      <v-card-text>
        <!-- What an admin must know BEFORE flipping the switch, not in an
             incident review afterwards. An SSH session is strictly more than
             a bounded command: it is interactive and it lasts. -->
        <v-alert
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-4"
          icon="mdi-shield-alert-outline"
        >
          Allowing SSH lets permitted users open an <strong>interactive
          shell</strong> on this device. It is a separate power from remote
          execution — a session lasts, and it is not one bounded command you
          can read back afterwards.
        </v-alert>

        <v-switch
          v-model="modeOn"
          color="primary"
          hide-details
          density="compact"
          label="Accept SSH sessions on this device"
        />

        <v-switch
          v-model="draft.can_originate"
          color="primary"
          hide-details
          density="compact"
          class="mt-2"
          label="Allow this device to open sessions on OTHER devices"
        />
        <div class="text-caption text-medium-emphasis mb-4 ml-10">
          Off by default. Without it, a compromised copy of this device could
          use its owner's permissions across the whole fleet.
        </div>

        <v-select
          v-model="draft.account_mode"
          :items="accountOptions"
          label="Sessions run as"
          density="compact"
          variant="outlined"
          class="mt-4"
          hide-details
        />
        <div class="text-caption text-medium-emphasis mb-4">
          <strong>The signed-in user</strong> is the least surprising choice on
          a workstation. <strong>The daemon account</strong> means
          <strong>SYSTEM</strong> on Windows and <strong>root</strong> under
          systemd — choosing it grants root on this device.
        </div>

        <v-text-field
          v-if="draft.account_mode === 'named'"
          v-model="namedAccount"
          label="Account name"
          density="compact"
          variant="outlined"
          class="mb-1"
          hide-details
          placeholder="e.g. deploy"
        />
        <div v-if="draft.account_mode === 'named'" class="text-caption text-medium-emphasis mb-4">
          Unix only — a Windows device refuses this mode rather than quietly
          falling back to another account.
        </div>

        <v-select
          v-model="draft.consent_mode"
          :items="consentOptions"
          label="Consent"
          density="compact"
          variant="outlined"
          class="mt-4"
          hide-details
        />
        <div class="text-caption text-medium-emphasis mb-4">
          <strong>Prompt</strong> asks whoever is at the device and denies
          after 30 s of silence — right for workstations.
          <strong>Unattended</strong> opens immediately — right for servers
          with nobody sitting at them. A denied prompt burns the grant, so the
          caller must request a new one rather than re-dialling.
        </div>

        <v-select
          v-model="draft.allowed_user_ids"
          :items="memberOptions"
          label="Restrict to these users (empty = anyone with the permission)"
          density="compact"
          variant="outlined"
          multiple
          chips
          closable-chips
          hide-details
          class="mb-4"
        />

        <!-- The two gates this dialog cannot set. Without these an admin
             flips everything here and then wonders why sessions still fail. -->
        <v-alert
          v-if="orgDisabled"
          type="info"
          variant="tonal"
          density="compact"
          class="mt-4"
        >
          SSH is switched off for the whole organization, so this device will
          still refuse. An org owner enables it in Settings. It is a separate
          switch from remote execution.
        </v-alert>
        <v-alert type="info" variant="tonal" density="compact" class="mt-4">
          The device itself must also allow it:
          <code>roomler config set ssh_enabled true</code> on the host, then
          restart the service. That switch belongs to whoever holds the machine
          and cannot be set from here.
        </v-alert>

        <v-alert
          v-if="error"
          type="error"
          variant="tonal"
          density="compact"
          class="mt-4"
        >
          {{ error }}
        </v-alert>
      </v-card-text>

      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="close">Cancel</v-btn>
        <v-btn color="primary" :loading="saving" @click="save">Save</v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useAgentStore, type Agent, type SshPolicy } from '@/stores/agents'

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
const saving = ref(false)
const error = ref('')
const agent = computed(() => props.agent)
const orgDisabled = computed(() => agentStore.orgSshEnabled === false)

/** A closed default, so a dialog opened on a device with no policy row can
 *  only ever be saved MORE permissive by an explicit click. `console_user`
 *  rather than `daemon`: if someone turns SSH on without reading the account
 *  selector, the quiet outcome must not be a root shell. */
function emptyPolicy(): SshPolicy {
  return {
    mode: 'off',
    can_originate: false,
    allowed_user_ids: [],
    allowed_role_ids: [],
    account_mode: 'console_user',
    account: null,
    consent_mode: null,
  }
}

const draft = ref<SshPolicy>(emptyPolicy())

/** Bound separately from `draft.mode` so the switch is a plain boolean; the
 *  wire type is an enum because `off | on` will grow and a bool could not. */
const modeOn = computed({
  get: () => draft.value.mode === 'on',
  set: (v: boolean) => {
    draft.value.mode = v ? 'on' : 'off'
  },
})

/** `account` is `string | null` on the wire but a text field yields `''`.
 *  Normalising here keeps an empty box from being saved as a named account
 *  with an empty name. */
const namedAccount = computed({
  get: () => draft.value.account ?? '',
  set: (v: string) => {
    draft.value.account = v.trim() === '' ? null : v
  },
})

const accountOptions = [
  { title: 'The signed-in user at the device', value: 'console_user' },
  { title: 'The daemon account (SYSTEM / root)', value: 'daemon' },
  { title: 'A named Unix account…', value: 'named' },
]

const consentOptions = [
  { title: 'Prompt at the device (recommended)', value: null },
  { title: 'Unattended — open immediately', value: 'auto' },
]

const memberOptions = computed(() =>
  agentStore.tenantMembers.map((m) => ({
    title: m.display_name || m.nickname || m.user_id,
    value: m.user_id,
  })),
)

watch(
  () => props.modelValue,
  (v) => {
    if (!v) return
    error.value = ''
    draft.value = { ...emptyPolicy(), ...(agent.value?.ssh_policy ?? {}) }
    if (!agentStore.tenantMembers.length) {
      void agentStore.fetchTenantMembers(props.tenantId)
    }
    if (agentStore.orgSshEnabled === null) {
      void agentStore.fetchOrgSshEnabled(props.tenantId)
    }
  },
)

async function save() {
  saving.value = true
  error.value = ''
  try {
    await agentStore.updateSshPolicy(props.tenantId, agent.value.id, draft.value)
    emit('saved')
    open.value = false
  } catch (e) {
    // The server refuses a prompt policy for an agent that would ignore it
    // (pre-P5d). Surfacing that here rather than closing is the point: the
    // admin must not walk away believing a prompt is enforced.
    error.value = e instanceof Error ? e.message : 'Could not save the policy.'
  } finally {
    saving.value = false
  }
}

function close() {
  open.value = false
}
</script>
