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
          Commands run as <strong>root</strong> (Linux) or, on Windows, as
          <strong>SYSTEM</strong> through the device's service once an
          administrator has anchored it. A Windows device that is not anchored
          runs them as the <strong>signed-in user</strong> on an attended
          install, and as SYSTEM in SystemContext mode. They are all recorded in
          the audit log.
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

        <!-- FR-94 — absent on a server from before it (`null`), so hidden
             rather than shown as a switch that cannot be read. -->
        <template v-if="agentStore.orgExecRequireSystem !== null">
          <v-switch
            :model-value="agentStore.orgExecRequireSystem === true"
            :loading="savingRequire"
            :disabled="savingRequire || agentStore.orgExecEnabled === null"
            color="primary"
            density="compact"
            hide-details
            class="mt-4"
            label="Windows: require SYSTEM"
            data-testid="exec-require-system"
            @update:model-value="onToggleRequireSystem"
          />
          <div class="text-caption text-medium-emphasis mt-1">
            A Windows device runs commands as SYSTEM once an administrator has
            anchored it (<code>roomlerd anchor create</code>, elevated). With
            this on, a device that is not anchored refuses commands instead of
            running them as whoever is signed in.
          </div>
          <v-alert
            v-if="requireError"
            type="error"
            variant="tonal"
            density="compact"
            class="mt-3"
          >
            {{ requireError }}
          </v-alert>
        </template>
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

    <!-- FR-92 — keep busy. Unlike the cards above this is ON by default
         (operator decision) and the switch is the org's DENY, shown as
         "allow" so on reads as on. The device enforces it; a deny also stops
         a keep busy already running. -->
    <v-card data-testid="org-keep-busy-card">
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-cursor-default-gesture-outline" color="primary" class="mr-2" />
        Keep busy
      </v-card-title>
      <v-card-text>
        <p class="text-body-2 mb-3">
          A controller with input can leave a device's pointer moving in a pattern so it stays
          active: no screensaver, idle lock or "Away". It pauses whenever someone uses the
          device. Some organizations do not allow this because it defeats a screen-lock policy.
        </p>
        <v-switch
          :model-value="agentStore.orgKeepBusyDenied === false"
          :loading="savingKeepBusy"
          :disabled="savingKeepBusy || agentStore.orgKeepBusyDenied === null"
          color="primary"
          density="compact"
          hide-details
          label="Allow keep busy in this organization"
          data-testid="org-keep-busy-switch"
          @update:model-value="onToggleKeepBusy"
        />
        <div class="text-caption text-medium-emphasis mt-1">
          On by default. Turning it off stops keep busy on every device in the organization,
          immediately for devices that are online and at the next connect for the rest. A
          device's owner can also turn it off on that device (<code>keep_busy_enabled</code>).
        </div>

        <!-- FR-92 P8 — the org's default pattern. A preference, never a
             policy: it never reaches a device, and a person's own last
             choice for a device wins over it in the viewer. -->
        <v-select
          :model-value="agentStore.orgKeepBusyDefaultPattern ?? 'circle'"
          :items="patternItems"
          :label="t('keepBusy.defaultPattern.label')"
          :loading="savingKeepBusyPattern"
          :disabled="savingKeepBusyPattern || agentStore.orgKeepBusyDenied === null"
          density="compact"
          hide-details
          class="mt-4"
          style="max-width: 320px"
          data-testid="org-keep-busy-default-pattern"
          @update:model-value="onDefaultPattern"
        />
        <div class="text-caption text-medium-emphasis mt-1">
          {{ t('keepBusy.defaultPattern.hint') }}
        </div>

        <v-alert v-if="keepBusyError" type="error" variant="tonal" density="compact" class="mt-3">
          {{ keepBusyError }}
        </v-alert>

        <v-divider class="my-4" />
        <!-- FR-92 P7 — the permanent history. MANAGE_AGENTS server-side; a
             refusal shows inside the list and ends nothing. -->
        <KeepBusyHistorySection :tenant-id="tenantId" />
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
import { useI18n } from 'vue-i18n'
import { useTenantStore } from '@/stores/tenant'
import { useAgentStore } from '@/stores/agents'
import { KEEP_BUSY_PATTERNS } from '@/composables/keepBusy'
import KeepBusyHistorySection from '@/components/admin/KeepBusyHistorySection.vue'

const props = defineProps<{ tenantId: string }>()
const { t } = useI18n()
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

// FR-94 — its own pair, so a failure here never renders under the switch above.
const savingRequire = ref(false)
const requireError = ref<string | null>(null)

async function onToggleRequireSystem(v: boolean | null) {
  savingRequire.value = true
  requireError.value = null
  try {
    await agentStore.setOrgExecRequireSystem(props.tenantId, v === true)
  } catch (e) {
    requireError.value = (e as Error).message
  } finally {
    savingRequire.value = false
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

// FR-92 — its own pair too. The switch shows "allow"; the server stores
// the deny.
const savingKeepBusy = ref(false)
const keepBusyError = ref<string | null>(null)

async function onToggleKeepBusy(allow: boolean | null) {
  savingKeepBusy.value = true
  keepBusyError.value = null
  try {
    await agentStore.setOrgKeepBusyDenied(props.tenantId, allow !== true)
  } catch (e) {
    keepBusyError.value = (e as Error).message
  } finally {
    savingKeepBusy.value = false
  }
}

// FR-92 P8 — the default pattern, its own saving flag so a failed pattern
// change never greys out the switch. The labels are the viewer's own.
const patternItems = KEEP_BUSY_PATTERNS.map((p) => ({ title: p.label, value: p.id }))
const savingKeepBusyPattern = ref(false)

async function onDefaultPattern(pattern: string | null) {
  if (!pattern) return
  savingKeepBusyPattern.value = true
  keepBusyError.value = null
  try {
    await agentStore.setOrgKeepBusyDefaultPattern(props.tenantId, pattern)
  } catch (e) {
    keepBusyError.value = (e as Error).message
  } finally {
    savingKeepBusyPattern.value = false
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
  void agentStore.fetchOrgKeepBusyDenied(props.tenantId)
  void agentStore.fetchOrgEphemeralKeysEnabled(props.tenantId)
})
</script>
