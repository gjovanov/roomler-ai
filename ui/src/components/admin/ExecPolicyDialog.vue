<template>
  <v-dialog v-model="open" max-width="720" scrollable>
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-shield-key-outline" color="primary" class="mr-2" />
        <span>Execution policy — {{ agent?.name }}</span>
      </v-card-title>

      <v-card-text>
        <!-- The single most important sentence in this dialog. An operator
             deciding whether to flip the switch needs to know it grants root,
             not read it in an incident review afterwards. -->
        <v-alert
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-4"
          icon="mdi-shield-alert-outline"
        >
          Allowing remote execution on this device lets permitted users run
          commands as <strong>SYSTEM</strong> (Windows) or <strong>root</strong>
          (Linux). It is a separate power from remote control, which is
          consent-gated, visible on screen, and runs as the signed-in user.
        </v-alert>

        <v-switch
          v-model="modeOn"
          color="primary"
          hide-details
          density="compact"
          label="Accept remote commands on this device"
        />

        <v-switch
          v-model="draft.can_originate"
          color="primary"
          hide-details
          density="compact"
          class="mt-2"
          label="Allow this device to run commands on OTHER devices (roomler exec)"
        />
        <div class="text-caption text-medium-emphasis mb-4 ml-10">
          Off by default. Without it, a compromised copy of this device could
          use its owner's permissions across the whole fleet.
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
          <strong>Prompt</strong> asks whoever is at the device and denies after
          30 s of silence — right for workstations.
          <strong>Unattended</strong> runs immediately — right for servers with
          nobody sitting at them.
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

        <v-select
          v-model="draft.shells"
          :items="shellOptions"
          label="Allowed shells (empty = any the host supports)"
          density="compact"
          variant="outlined"
          multiple
          chips
          closable-chips
          hide-details
        />

        <!-- Two gates this dialog cannot set, surfaced so an admin doesn't
             flip everything here and then wonder why commands still fail. -->
        <v-alert
          v-if="orgDisabled"
          type="info"
          variant="tonal"
          density="compact"
          class="mt-4"
        >
          Remote execution is switched off for the whole organization, so this
          device will still refuse. An org owner enables it in Settings.
        </v-alert>
        <v-alert type="info" variant="tonal" density="compact" class="mt-4">
          The device itself must also allow it:
          <code>roomler config set exec_enabled true</code> on the host, then
          restart the service. That switch belongs to whoever holds the machine
          and cannot be set from here.
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
import { useAgentStore, type Agent, type ExecPolicy } from '@/stores/agents'

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
const agent = computed(() => props.agent)
const orgDisabled = computed(() => agentStore.orgExecEnabled === false)

/** A closed default, so a dialog opened on a device with no policy row can
 *  only ever be saved MORE permissive by an explicit click. */
function emptyPolicy(): ExecPolicy {
  return {
    mode: 'off',
    can_originate: false,
    allowed_user_ids: [],
    allowed_role_ids: [],
    consent_mode: null,
    shells: [],
  }
}

const draft = ref<ExecPolicy>(emptyPolicy())

/** Bound separately from `draft.mode` so the switch is a plain boolean; the
 *  wire type is an enum because `off | on` will grow (e.g. a read-only verb
 *  tier) and a bool could not. */
const modeOn = computed({
  get: () => draft.value.mode === 'on',
  set: (v: boolean) => {
    draft.value.mode = v ? 'on' : 'off'
  },
})

const consentOptions = [
  { title: 'Prompt at the device (recommended)', value: null },
  { title: 'Unattended — run immediately', value: 'auto' },
]

const memberOptions = computed(() =>
  agentStore.tenantMembers.map((m) => ({
    title: m.display_name || m.nickname || m.user_id,
    value: m.user_id,
  })),
)

const shellOptions = computed(() =>
  agent.value?.os === 'windows' ? ['powershell', 'pwsh', 'cmd'] : ['bash', 'sh'],
)

watch(
  () => props.modelValue,
  (v) => {
    if (!v) return
    draft.value = { ...emptyPolicy(), ...(agent.value?.exec_policy ?? {}) }
    if (!agentStore.tenantMembers.length) {
      void agentStore.fetchTenantMembers(props.tenantId)
    }
    if (agentStore.orgExecEnabled === null) {
      void agentStore.fetchOrgExecEnabled(props.tenantId)
    }
  },
)

async function save() {
  saving.value = true
  try {
    await agentStore.updateExecPolicy(props.tenantId, agent.value.id, draft.value)
    emit('saved')
    open.value = false
  } finally {
    saving.value = false
  }
}

function close() {
  open.value = false
}
</script>
