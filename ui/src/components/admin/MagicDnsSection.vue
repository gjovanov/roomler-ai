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
        class="mb-4"
      />

      <v-text-field
        v-model="nameserversText"
        label="Upstream nameservers"
        placeholder="1.1.1.1, 8.8.8.8"
        hint="Comma-separated. Blank uses each node's existing system resolvers."
        persistent-hint
        class="mb-4"
      />

      <v-btn color="primary" variant="flat" :loading="loading" @click="save">
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

async function load() {
  try {
    const s = await store.fetchMagicDns(props.tenantId)
    domain.value = s.magic_dns_domain ?? ''
    nameserversText.value = s.magic_dns_nameservers.join(', ')
  } catch (e) {
    error.value = (e as Error).message
  }
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
    error.value = (e as Error).message
  } finally {
    loading.value = false
  }
}

load()
</script>
