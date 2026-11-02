<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- A device's access-policy controls — ONE copy for the desktop grid's
       Consent cell (`plain`: values only, stacked in the cell) and the phone
       card's Access panel (`outlined`: labelled fields). Every control writes
       on change; the parent owns the request and the busy state. -->
  <div :class="['device-policy-fields', `device-policy-fields--${variant}`]">
    <v-select
      :model-value="agent.access_policy.consent_mode ?? 'prompt'"
      :items="CONSENT_MODE_ITEMS"
      :label="labelled ? 'Consent' : undefined"
      density="compact"
      :variant="variant"
      hide-details
      :disabled="busy"
      :loading="busy"
      class="consent-select"
      :aria-label="`Consent mode for ${deviceName}`"
      @update:model-value="(m) => emit('consent', m as ConsentMode)"
    />
    <!-- FR-27 — the owner shortcut, made visible. Controlling your OWN device
         auto-consents server-side BEFORE this mode is read, so on a fleet
         where one person owns everything the select above had no observable
         effect and nothing said so. Shown only on devices YOU own; for
         everyone else the mode already applies and the row would be noise. -->
    <v-checkbox
      v-if="ownedByMe"
      :model-value="agent.access_policy.prompt_owner ?? false"
      density="compact"
      hide-details
      :disabled="busy"
      :label="ownerPromptLabel(agent.access_policy)"
      :aria-label="`Apply the consent mode to me, the owner of ${deviceName}`"
      class="consent-owner-toggle"
      @update:model-value="(v) => emit('prompt-owner', v === true)"
    />
    <!-- P6 — multi-user input mode (free-for-all | exclusive). -->
    <v-select
      :model-value="agent.access_policy.input_mode ?? 'free'"
      :items="INPUT_MODE_ITEMS"
      :label="labelled ? 'Input' : undefined"
      density="compact"
      :variant="variant"
      hide-details
      :disabled="busy"
      class="consent-select"
      :aria-label="`Input mode for ${deviceName}`"
      @update:model-value="(m) => emit('input-mode', m as 'free' | 'exclusive')"
    />
    <!-- FR-101 P4b-2b — this device's own idle limit. The shorter of it and
         the organization's applies, so it can tighten the org's but never
         loosen it; the device keeps it. -->
    <v-select
      :model-value="agent.access_policy.auto_terminate_idle_minutes ?? 0"
      :items="idleMinuteItems(agent.access_policy.auto_terminate_idle_minutes)"
      :label="labelled ? 'Idle limit' : undefined"
      density="compact"
      :variant="variant"
      hide-details
      :disabled="busy"
      class="consent-select"
      :title="IDLE_LIMIT_HINT"
      :aria-label="`Idle limit for ${deviceName}`"
      @update:model-value="(m) => emit('idle-minutes', m as number)"
    />
    <!-- FR-99 P3b — ask this device to lock its screen when the last remote
         session ends. The device takes the stricter of this and its owner's
         own setting, so it can only tighten it. -->
    <v-select
      :model-value="agent.access_policy.lock_on_end ?? 'never'"
      :items="LOCK_ON_END_ITEMS"
      :label="labelled ? 'Lock when sessions end' : undefined"
      density="compact"
      :variant="variant"
      hide-details
      :disabled="busy"
      class="consent-select"
      :title="LOCK_ON_END_HINT"
      :aria-label="`Lock ${deviceName} when its remote sessions end`"
      @update:model-value="(m) => emit('lock-on-end', m as LockOnEnd)"
    />
    <!-- #1882 — may a session to this device ride the mesh? Off by default:
         the agent keeps overlay addresses out of its ICE. FR-128 — Auto lets
         each session decide from the mesh link to the viewer's own device. -->
    <v-select
      :model-value="rcOverlayMode(agent.access_policy)"
      :items="RC_OVERLAY_ITEMS"
      :label="labelled ? 'Remote desktop over the mesh' : undefined"
      density="compact"
      :variant="variant"
      hide-details
      :disabled="busy"
      class="consent-select"
      :title="RC_OVERLAY_HINT"
      :aria-label="`Remote desktop to ${deviceName} over the mesh (overlay)`"
      @update:model-value="(m) => emit('rc-overlay', m as RcOverlayMode)"
    />
    <!-- The hints above live in `title`, which a touch screen never shows.
         The labelled (phone) layout spells them out instead, on demand. -->
    <div v-if="labelled" class="text-caption text-medium-emphasis mt-1">
      <a
        href="#"
        class="text-primary"
        :aria-expanded="showHints"
        @click.prevent="showHints = !showHints"
      >
        {{ showHints ? 'Hide what these do' : 'What do these do?' }}
      </a>
      <ul v-if="showHints" class="ps-4 mt-1">
        <li class="mb-1"><strong>Idle limit</strong> — {{ IDLE_LIMIT_HINT }}</li>
        <li class="mb-1"><strong>Lock when sessions end</strong> — {{ LOCK_ON_END_HINT }}</li>
        <li><strong>Remote desktop over the mesh</strong> — {{ RC_OVERLAY_HINT }}</li>
      </ul>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref } from 'vue'
import type { Agent, ConsentMode, LockOnEnd, RcOverlayMode } from '@/stores/agents'
import {
  CONSENT_MODE_ITEMS,
  IDLE_LIMIT_HINT,
  INPUT_MODE_ITEMS,
  LOCK_ON_END_HINT,
  LOCK_ON_END_ITEMS,
  RC_OVERLAY_HINT,
  RC_OVERLAY_ITEMS,
  idleMinuteItems,
  ownerPromptLabel,
  rcOverlayMode,
} from './devicePolicy'

const props = withDefaults(
  defineProps<{
    agent: Agent
    /** A write for this device is in flight: every control waits. */
    busy?: boolean
    /** The caller owns the device — only then does the owner toggle exist. */
    ownedByMe?: boolean
    /** `plain` for the grid cell (values only), `outlined` for the phone
     *  card (labelled fields). */
    variant?: 'plain' | 'outlined'
  }>(),
  { busy: false, ownedByMe: false, variant: 'plain' },
)

const emit = defineEmits<{
  consent: [mode: ConsentMode]
  'prompt-owner': [on: boolean]
  'input-mode': [mode: 'free' | 'exclusive']
  'idle-minutes': [minutes: number]
  'lock-on-end': [lock: LockOnEnd]
  'rc-overlay': [mode: RcOverlayMode]
}>()

const labelled = computed(() => props.variant === 'outlined')
const deviceName = computed(() => props.agent.display_name || props.agent.name)
const showHints = ref(false)
</script>

<style scoped>
.device-policy-fields--outlined {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
</style>
