<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-card>
    <v-card-title>MagicDNS</v-card-title>

    <v-card-text>
      <v-alert
        v-if="error"
        type="error"
        variant="tonal"
        closable
        @click:close="error = null"
        class="mb-4"
      >
        {{ error }}
      </v-alert>

      <v-alert
        v-if="saved"
        type="success"
        variant="tonal"
        closable
        @click:close="saved = false"
        class="mb-4"
      >
        Saved. Overlay nodes pick up the change on their next join.
      </v-alert>

      <!-- Changing MagicDNS is MANAGE_AGENTS on the server: every node of the
           org applies it, its whole DNS under an exit node. Reading it is any
           member's, so a caller without the bit sees the settings read-only;
           a save's 403 lands here too, never as an error. -->
      <v-alert
        v-if="!canEdit"
        type="info"
        variant="tonal"
        class="mb-4"
        data-testid="magic-dns-refused"
      >
        MagicDNS is changed by people with the <strong>Manage devices</strong>
        permission. These are the organization's current settings.
      </v-alert>

      <p class="text-medium-emphasis mb-4">
        Give the mesh a DNS suffix so nodes are reachable by name
        (<code>&lt;node&gt;.&lt;domain&gt;</code>) instead of overlay IP. Each node
        runs a local split-DNS resolver: overlay names resolve to overlay IPs,
        everything else forwards to the upstream nameservers below. Leave the
        domain blank to disable MagicDNS.
      </p>

      <v-text-field
        v-model="domain"
        label="MagicDNS domain"
        placeholder="myorg.roomler.net"
        hint="Blank disables MagicDNS for the tenant."
        persistent-hint
        :readonly="!canEdit"
        class="mb-4"
      />

      <v-text-field
        v-model="nameserversText"
        label="Upstream nameservers"
        placeholder="1.1.1.1, 8.8.8.8"
        hint="Comma-separated. Blank uses each node's existing system resolvers."
        persistent-hint
        :readonly="!canEdit"
        class="mb-4"
      />

      <v-btn v-if="canEdit" color="primary" variant="flat" :loading="loading" @click="save">
        Save
      </v-btn>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import { useOverlayRoutesStore } from '@/stores/overlayRoutes'

const props = defineProps<{ tenantId: string }>()
const store = useOverlayRoutesStore()

const domain = ref('')
const nameserversText = ref('')
const loading = ref(false)
const saved = ref(false)
const error = ref<string | null>(null)
/** Whether this caller may change the settings: the mask's answer first, and
 *  a save the server refuses turns it off. */
const canEdit = ref(true)

/** FR-82 — a 403 is an answer, "you may not", never a failed session. */
function isRefusal(e: unknown): boolean {
  return typeof e === 'object' && e !== null && (e as { status?: unknown }).status === 403
}

async function loadSettings() {
  try {
    const s = await store.fetchMagicDns(props.tenantId)
    domain.value = s.magic_dns_domain ?? ''
    nameserversText.value = s.magic_dns_nameservers.join(', ')
  } catch (e) {
    error.value = (e as Error).message
  }
}

async function load() {
  canEdit.value = await store.mayEditMagicDns(props.tenantId)
  await loadSettings()
}

async function save() {
  loading.value = true
  error.value = null
  saved.value = false
  try {
    const nameservers = nameserversText.value
      .split(',')
      .map((s) => s.trim())
      .filter((s) => s.length > 0)
    const trimmed = domain.value.trim()
    const s = await store.saveMagicDns(props.tenantId, {
      magic_dns_domain: trimmed.length > 0 ? trimmed : null,
      magic_dns_nameservers: nameservers,
    })
    domain.value = s.magic_dns_domain ?? ''
    nameserversText.value = s.magic_dns_nameservers.join(', ')
    saved.value = true
  } catch (e) {
    if (isRefusal(e)) {
      // Refused: show what the org has, read-only, rather than an error.
      canEdit.value = false
      await loadSettings()
    } else {
      error.value = (e as Error).message
    }
  } finally {
    loading.value = false
  }
}

load()
</script>
