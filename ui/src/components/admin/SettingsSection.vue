<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <div class="d-flex flex-column ga-4">
    <v-card>
      <v-card-title>Workspace Settings</v-card-title>
      <v-card-text>
        <v-text-field
          :model-value="tenantStore.current?.name"
          label="Workspace Name"
          disabled
        />
        <v-text-field
          :model-value="tenantStore.current?.slug"
          label="Slug"
          disabled
        />
      </v-card-text>
    </v-card>

    <!-- Fleet RPC gate 1. Deliberately here rather than in the device list:
         this one switch governs every device in the org, so it belongs with
         org settings and needs MANAGE_TENANT, not fleet-admin rights. -->
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-console-network-outline" color="primary" class="mr-2" />
        Remote command execution
      </v-card-title>
      <v-card-text>
        <v-alert
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-4"
          icon="mdi-shield-alert-outline"
        >
          When enabled, users holding the <strong>EXEC_DEVICE</strong>
          permission can run commands on devices that individually opt in.
          Commands run as <strong>SYSTEM</strong> (Windows) or
          <strong>root</strong> (Linux) and are all recorded in the audit log
          below.
        </v-alert>

        <v-switch
          :model-value="agentStore.orgExecEnabled === true"
          :loading="saving"
          :disabled="saving || agentStore.orgExecEnabled === null"
          color="primary"
          density="compact"
          hide-details
          label="Allow remote command execution in this organization"
          @update:model-value="onToggle"
        />
        <div class="text-caption text-medium-emphasis mt-1">
          Off by default. Turning it on does not enable any device on its own —
          each device must also be allowed individually, and the device's own
          <code>exec_enabled</code> setting must permit it.
        </div>

        <v-alert v-if="error" type="error" variant="tonal" density="compact" class="mt-3">
          {{ error }}
        </v-alert>
      </v-card-text>
    </v-card>

    <!-- Roomler SSH gate 1. A SEPARATE card from remote execution above, and
         a separate switch server-side: allowing bounded diagnostic commands
         is not the same decision as allowing interactive sessions, and one
         control must never read as the other. -->
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-console-network-outline" color="primary" class="mr-2" />
        Roomler SSH
      </v-card-title>
      <v-card-text>
        <v-alert
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-4"
          icon="mdi-shield-alert-outline"
        >
          When enabled, users holding the <strong>SSH_DEVICE</strong>
          permission can open an interactive shell on devices that
          individually opt in. A session lasts, unlike a single command — and
          which account it runs as is set per device.
        </v-alert>

        <v-switch
          :model-value="agentStore.orgSshEnabled === true"
          :loading="savingSsh"
          :disabled="savingSsh || agentStore.orgSshEnabled === null"
          color="primary"
          density="compact"
          hide-details
          label="Allow SSH sessions in this organization"
          @update:model-value="onToggleSsh"
        />
        <div class="text-caption text-medium-emphasis mt-1">
          Off by default. Turning it on does not enable any device on its own —
          each device must also be allowed individually, and the device's own
          <code>ssh_enabled</code> setting must permit it.
        </div>

        <v-alert v-if="sshError" type="error" variant="tonal" density="compact" class="mt-3">
          {{ sshError }}
        </v-alert>
      </v-card-text>
    </v-card>

    <!-- FR-52 gate 1. A THIRD card, and a third server-side switch, for a
         reason stronger than the one separating the two above: those decide
         what a MEMBER of this organization may do, and this one decides
         whether someone who is not a member may do anything at all. -->
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-account-arrow-right-outline" color="primary" class="mr-2" />
        External access
      </v-card-title>
      <v-card-text>
        <v-alert
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-4"
          icon="mdi-shield-account-outline"
        >
          When enabled, people <strong>outside this organization</strong> can
          control devices that individually opt in — the way a support
          technician reaches a machine they do not administer. They need the
          device's connect code and a password held
          <strong>on the device itself</strong>, which this server never sees.
        </v-alert>

        <v-switch
          :model-value="agentStore.orgExternalAccessEnabled === true"
          :loading="savingExternal"
          :disabled="savingExternal || agentStore.orgExternalAccessEnabled === null"
          color="primary"
          density="compact"
          hide-details
          label="Allow people outside this organization to control devices"
          @update:model-value="onToggleExternal"
        />
        <div class="text-caption text-medium-emphasis mt-1">
          Off by default, and turning it on opens nothing by itself. Each
          device must be approved individually, its owner must enable it on
          the machine and set a password there, and whoever is at the device
          is still asked before a session starts.
        </div>

        <v-alert v-if="externalError" type="error" variant="tonal" density="compact" class="mt-3">
          {{ externalError }}
        </v-alert>
      </v-card-text>
    </v-card>

    <!-- FR-51 gate 1. Its own card and its own server-side switch, like the
         two above: a standing credential that mints device identities is its
         own grant, not an implication of exec or SSH. -->
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-clock-fast" color="primary" class="mr-2" />
        Ephemeral enrollment keys
      </v-card-title>
      <v-card-text>
        <v-alert
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-4"
          icon="mdi-shield-alert-outline"
        >
          When enabled, fleet admins can mint <strong>reusable</strong>
          enrollment keys that create <strong>self-removing</strong> devices
          (CI runners, containers). A key is a standing credential: anyone
          holding it can enroll devices into this organization until it is
          revoked, exhausted, or expired.
        </v-alert>

        <v-switch
          :model-value="agentStore.orgEphemeralKeysEnabled === true"
          :loading="savingKeys"
          :disabled="savingKeys || agentStore.orgEphemeralKeysEnabled === null"
          color="primary"
          density="compact"
          hide-details
          label="Allow ephemeral enrollment keys in this organization"
          @update:model-value="onToggleKeys"
        />
        <div class="text-caption text-medium-emphasis mt-1">
          Off by default. Turning it OFF is an org-wide revocation: every
          outstanding key stops working on its next use. Devices already
          minted are untouched — they remove themselves by their own deadline.
        </div>

        <v-alert v-if="keysError" type="error" variant="tonal" density="compact" class="mt-3">
          {{ keysError }}
        </v-alert>
      </v-card-text>
    </v-card>

    <!-- The exec/SSH audit + activity sections moved to /tenant/{id}/audit
         (2026-08-26) — history review is its own nav destination now. -->
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useTenantStore } from '@/stores/tenant'
import { useAgentStore } from '@/stores/agents'

const props = defineProps<{ tenantId: string }>()
const tenantStore = useTenantStore()
const agentStore = useAgentStore()

const saving = ref(false)
const error = ref<string | null>(null)

// Tracked separately from the exec pair on purpose: a failure to flip one
// switch must not render as an error under the other.
const savingSsh = ref(false)
const sshError = ref<string | null>(null)

async function onToggle(v: boolean | null) {
  saving.value = true
  error.value = null
  try {
    await agentStore.setOrgExecEnabled(props.tenantId, v === true)
  } catch (e) {
    error.value = (e as Error).message
  } finally {
    saving.value = false
  }
}

async function onToggleSsh(v: boolean | null) {
  savingSsh.value = true
  sshError.value = null
  try {
    await agentStore.setOrgSshEnabled(props.tenantId, v === true)
  } catch (e) {
    sshError.value = (e as Error).message
  } finally {
    savingSsh.value = false
  }
}

// FR-52 — its own pair too. A failure to flip the external-access switch
// must never render as an error under exec or SSH.
const savingExternal = ref(false)
const externalError = ref<string | null>(null)

async function onToggleExternal(v: boolean | null) {
  savingExternal.value = true
  externalError.value = null
  try {
    await agentStore.setOrgExternalAccessEnabled(props.tenantId, v === true)
  } catch (e) {
    externalError.value = (e as Error).message
  } finally {
    savingExternal.value = false
  }
}

// FR-51 — its own pair, same isolation rule as exec vs SSH above.
const savingKeys = ref(false)
const keysError = ref<string | null>(null)

async function onToggleKeys(v: boolean | null) {
  savingKeys.value = true
  keysError.value = null
  try {
    await agentStore.setOrgEphemeralKeysEnabled(props.tenantId, v === true)
  } catch (e) {
    keysError.value = (e as Error).message
  } finally {
    savingKeys.value = false
  }
}

onMounted(() => {
  void agentStore.fetchOrgExecEnabled(props.tenantId)
  void agentStore.fetchOrgSshEnabled(props.tenantId)
  void agentStore.fetchOrgEphemeralKeysEnabled(props.tenantId)
  void agentStore.fetchExternalAccess(props.tenantId)
})
</script>
