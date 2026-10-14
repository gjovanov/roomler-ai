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

    <!-- FR-101 P2 — what a remote-desktop session in this org may carry.
         Each feature on / ask / off; the stricter of this and a device's own
         setting wins, and the device enforces it whatever the server sends.
         A change applies to sessions that start after it. -->
    <v-card data-testid="org-rc-session-card">
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-shield-lock-outline" color="primary" class="mr-2" />
        Remote session features
      </v-card-title>
      <v-card-text>
        <p class="text-body-2 mb-3">
          Choose what a remote-desktop session in this organization may use. <strong>Ask</strong>
          allows a feature only when the person at the device approves the session on a prompt
          that names it; a session nobody there is asked about (an unattended device, your own
          device, an administrator's forced session) goes without it.
        </p>
        <v-select
          v-for="f in rcFeatureRows"
          :key="f.key"
          :model-value="agentStore.orgRcSessionPolicy?.[f.key] ?? null"
          :items="rcPolicyItems"
          :label="f.label"
          :loading="savingRcFeature === f.key"
          :disabled="savingRcFeature !== null || agentStore.orgRcSessionPolicy === null"
          density="compact"
          hide-details
          class="mb-3"
          style="max-width: 320px"
          :data-testid="`org-rc-session-${f.key}`"
          @update:model-value="(v: RcFeaturePolicy | null) => onRcFeature(f.key, v)"
        />
        <div class="text-caption text-medium-emphasis mt-1">
          On by default. A device's owner can also turn each one off on that device
          (<code>rc_clipboard</code>, <code>rc_files</code>, <code>rc_audio</code>). A change applies
          to sessions that start after it.
        </div>
        <v-alert v-if="rcSessionError" type="error" variant="tonal" density="compact" class="mt-3">
          {{ rcSessionError }}
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

    <!-- FR-101 P4b — how long a remote-desktop session in this org may last.
         The hub ends a session that reaches it (the viewer is told which limit
         it was). Off for existing organizations (operator decision). -->
    <v-card data-testid="org-rc-limits-card">
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-timer-sand" color="primary" class="mr-2" />
        Remote session limits
      </v-card-title>
      <v-card-text>
        <v-select
          :model-value="agentStore.orgRcMaxSessionSecs === undefined ? null : agentStore.orgRcMaxSessionSecs ?? 0"
          :items="maxSessionOptions()"
          label="Maximum session length"
          :loading="savingMaxSession"
          :disabled="savingMaxSession || agentStore.orgRcMaxSessionSecs === undefined"
          density="compact"
          hide-details
          style="max-width: 320px"
          data-testid="org-rc-max-session"
          @update:model-value="onMaxSession"
        />
        <div class="text-caption text-medium-emphasis mt-1">
          A session ends this long after it started, and the person controlling it is told why.
          No limit by default. A change applies to sessions that start after it.
        </div>
        <v-alert v-if="maxSessionError" type="error" variant="tonal" density="compact" class="mt-3">
          {{ maxSessionError }}
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
import { useAgentStore, type RcFeaturePolicy, type RcSessionPolicy } from '@/stores/agents'
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

// FR-101 P2 — the org's session policy, one select per feature. Its own error,
// and the feature being saved, so a failure never renders under another card.
const rcFeatureRows: { key: keyof RcSessionPolicy; label: string }[] = [
  { key: 'clipboard', label: 'Clipboard' },
  { key: 'files', label: 'File transfer' },
  { key: 'audio', label: "The device's audio" },
]
const rcPolicyItems: { title: string; value: RcFeaturePolicy }[] = [
  { title: 'On', value: 'on' },
  { title: 'Ask the person at the device', value: 'ask' },
  { title: 'Off', value: 'off' },
]
const savingRcFeature = ref<keyof RcSessionPolicy | null>(null)
const rcSessionError = ref<string | null>(null)

async function onRcFeature(key: keyof RcSessionPolicy, value: RcFeaturePolicy | null) {
  if (!value) return
  savingRcFeature.value = key
  rcSessionError.value = null
  try {
    await agentStore.setOrgRcSessionPolicy(props.tenantId, { [key]: value })
  } catch (e) {
    rcSessionError.value = (e as Error).message
  } finally {
    savingRcFeature.value = null
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

// FR-101 P4b — the maximum session length. `0` in the select is "no limit"
// (sent as `null`); the stored value is seconds.
const maxSessionItems: { title: string; value: number }[] = [
  { title: 'No limit', value: 0 },
  { title: '30 minutes', value: 30 * 60 },
  { title: '1 hour', value: 60 * 60 },
  { title: '2 hours', value: 2 * 60 * 60 },
  { title: '4 hours', value: 4 * 60 * 60 },
  { title: '8 hours', value: 8 * 60 * 60 },
  { title: '24 hours', value: 24 * 60 * 60 },
]
/** The presets, plus the stored value when it is none of them (set through
 *  the API), so the select never shows blank for a limit that is in force. */
function maxSessionOptions(): { title: string; value: number }[] {
  const cur = agentStore.orgRcMaxSessionSecs
  if (typeof cur === 'number' && cur > 0 && !maxSessionItems.some((i) => i.value === cur)) {
    return [...maxSessionItems, { title: `${Math.round(cur / 60)} minutes`, value: cur }]
  }
  return maxSessionItems
}
const savingMaxSession = ref(false)
const maxSessionError = ref<string | null>(null)

async function onMaxSession(secs: number | null) {
  if (secs === null) return
  savingMaxSession.value = true
  maxSessionError.value = null
  try {
    await agentStore.setOrgRcMaxSessionSecs(props.tenantId, secs === 0 ? null : secs)
  } catch (e) {
    maxSessionError.value = (e as Error).message
  } finally {
    savingMaxSession.value = false
  }
}

onMounted(() => {
  void agentStore.fetchOrgRcSessionLimits(props.tenantId)
  void agentStore.fetchOrgExecEnabled(props.tenantId)
  void agentStore.fetchOrgSshEnabled(props.tenantId)
  void agentStore.fetchOrgKeepBusyDenied(props.tenantId)
  void agentStore.fetchOrgRcSessionPolicy(props.tenantId)
  void agentStore.fetchOrgEphemeralKeysEnabled(props.tenantId)
})
</script>
