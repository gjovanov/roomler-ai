<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-card variant="outlined">
    <v-card-title class="d-flex align-center">
      <v-icon icon="mdi-account-multiple-outline" color="primary" class="mr-2" />
      <span>People from outside</span>
      <v-spacer />
      <v-btn
        icon="mdi-refresh"
        size="small"
        variant="text"
        :loading="loading"
        aria-label="Refresh"
        @click="load"
      />
    </v-card-title>

    <v-card-subtitle class="pb-2 text-wrap">
      Everyone who has asked for a session on one of this organization's devices
      from outside it, device by device. Kept for as long as the organization is.
    </v-card-subtitle>

    <v-card-text>
      <v-alert v-if="error" type="error" variant="tonal" density="compact" class="mb-3">
        {{ error }}
      </v-alert>

      <!-- What a bar is worth, said before anyone leans on it: it is a server
           gate on an account (FR-52 §3), and the password is the device's. -->
      <v-alert type="info" variant="tonal" density="compact" class="mb-3">
        <strong>Bar</strong> ends this account's session on that device and refuses
        its next login. It stops an account, not a person: anyone who knows the
        password can come back on another one. To shut everyone out, change the
        password on the device. Names are whatever the person calls themselves.
      </v-alert>

      <div v-if="!loading && !people.length" class="text-medium-emphasis text-caption">
        Nobody has come in from outside yet.
      </div>

      <template v-else>
        <v-table density="compact" class="table-stack-xs table-stack-xs--compact">
          <thead>
            <tr>
              <th>Who</th>
              <th>Device</th>
              <th>First session</th>
              <th>Last session</th>
              <th>Standing</th>
              <th aria-label="Action" />
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="p in visible"
              :key="rowKey(p)"
              :class="{ 'stack-open': isOpen(rowKey(p)) }"
              data-testid="external-person"
            >
              <td v-if="xs">
                <button
                  type="button"
                  class="stack-toggle"
                  :aria-expanded="isOpen(rowKey(p))"
                  data-testid="audit-row-toggle"
                  @click="toggle(rowKey(p))"
                >
                  <span class="stack-toggle__text">
                    <span class="font-weight-medium">{{ deviceName(p.agent_id) }}</span>
                    <span class="text-caption text-medium-emphasis"> · {{ personName(p) }}</span>
                  </span>
                  <v-chip
                    size="x-small"
                    :color="p.revoked_at ? 'error' : 'info'"
                    variant="flat"
                    class="flex-shrink-0"
                  >
                    {{ standing(p) }}
                  </v-chip>
                  <v-icon size="small" class="flex-shrink-0" aria-hidden="true">
                    {{ isOpen(rowKey(p)) ? 'mdi-chevron-up' : 'mdi-chevron-down' }}
                  </v-icon>
                </button>
              </td>
              <td data-label="Who" class="stack-detail">
                <span class="text-no-wrap">{{ personName(p) }}</span>
                <!-- Two accounts can carry one name: the id tells them apart. -->
                <v-chip size="x-small" variant="tonal" class="ml-1" :title="`Account ${p.user_id}`">
                  outside · {{ p.user_id.slice(-6) }}
                </v-chip>
              </td>
              <td v-if="!xs" data-label="Device" class="text-no-wrap">{{ deviceName(p.agent_id) }}</td>
              <td data-label="First session" class="text-no-wrap stack-detail">
                {{ fmtWhen(p.first_session_at) }}
              </td>
              <td data-label="Last session" class="text-no-wrap stack-detail">
                {{ fmtWhen(p.last_session_at) }}
              </td>
              <td v-if="!xs" data-label="Standing" class="text-no-wrap">
                <v-chip
                  size="x-small"
                  :color="p.revoked_at ? 'error' : 'info'"
                  variant="flat"
                  :title="p.revoked_at ? `Since ${fmtWhen(p.revoked_at)}` : undefined"
                >
                  {{ standing(p) }}
                </v-chip>
              </td>
              <td class="text-no-wrap stack-detail">
                <v-btn
                  v-if="p.revoked_at ? mayLetBack : mayBar"
                  size="x-small"
                  variant="tonal"
                  :color="p.revoked_at ? undefined : 'error'"
                  :loading="busy === rowKey(p)"
                  :data-testid="p.revoked_at ? 'external-let-back' : 'external-bar'"
                  @click="setBarred(p, !p.revoked_at)"
                >
                  {{ p.revoked_at ? 'Let back' : 'Bar' }}
                </v-btn>
              </td>
            </tr>
          </tbody>
        </v-table>
        <div v-if="remaining" class="text-center mt-2">
          <v-btn variant="tonal" size="small" data-testid="audit-show-more" @click="showMore">
            Show {{ nextCount }} more
          </v-btn>
        </div>
      </template>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useDisplay } from 'vuetify'
import { useAgentStore } from '@/stores/agents'
import { useTenantStore } from '@/stores/tenant'
import { useNodeLabel } from '@/composables/useNodeLabel'
import { useOpenRows } from '@/composables/useOpenRows'
import { usePhoneSlice } from '@/composables/usePhoneSlice'
import { fmtWhen, type ExternalPrincipal } from '@/utils/externalAudit'
import { canBarExternalPerson, canLetBackExternalPerson } from '@/utils/permissions'

const props = defineProps<{ tenantId: string }>()
const emit = defineEmits<{ changed: [] }>()

const agentStore = useAgentStore()
const tenantStore = useTenantStore()
const { agentLabel } = useNodeLabel()
const people = ref<ExternalPrincipal[]>([])
const { xs } = useDisplay()
const { visible, remaining, nextCount, showMore } = usePhoneSlice(people)
const { isOpen, toggle } = useOpenRows<string>()
const loading = ref(false)
const busy = ref<string | null>(null)
const error = ref<string | null>(null)

const mayBar = computed(() => canBarExternalPerson(tenantStore.myPermissions, tenantStore.isOwner))
const mayLetBack = computed(() =>
  canLetBackExternalPerson(tenantStore.myPermissions, tenantStore.isOwner),
)

function deviceName(id: string): string {
  return agentLabel(id, id.slice(0, 8))
}

function personName(p: ExternalPrincipal): string {
  return p.name || 'Account removed'
}

function standing(p: ExternalPrincipal): string {
  return p.revoked_at ? 'barred' : 'can log in'
}

function rowKey(p: ExternalPrincipal): string {
  return `${p.agent_id}:${p.user_id}`
}

async function load() {
  loading.value = true
  error.value = null
  try {
    const res = await agentStore.fetchExternalPrincipals(props.tenantId, { perPage: 100 })
    people.value = res.items
  } catch (e) {
    error.value = (e as Error).message
    people.value = []
  } finally {
    loading.value = false
  }
}

/** A refusal shows its message and changes nothing here: a 403 is an answer. */
async function setBarred(p: ExternalPrincipal, revoked: boolean) {
  busy.value = rowKey(p)
  error.value = null
  try {
    await agentStore.setExternalPrincipal(props.tenantId, p.agent_id, p.user_id, revoked)
    await load()
    emit('changed')
  } catch (e) {
    error.value = (e as Error).message
  } finally {
    busy.value = null
  }
}

onMounted(() => void load())

defineExpose({ load })
</script>
