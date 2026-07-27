<template>
  <v-card>
    <v-card-title class="d-flex align-center">
      <span>Machines</span>
      <v-spacer />
      <v-btn
        prepend-icon="mdi-refresh"
        variant="tonal"
        size="small"
        :loading="store.loading"
        @click="store.fetchNodes(tenantId)"
      >
        Refresh
      </v-btn>
    </v-card-title>

    <v-card-text>
      <v-alert
        v-if="store.error"
        type="error"
        variant="tonal"
        closable
        class="mb-4"
        @click:close="store.error = null"
      >
        {{ store.error }}
      </v-alert>

      <div v-if="store.loading && store.nodes.length === 0" class="d-flex justify-center pa-8">
        <v-progress-circular indeterminate />
      </div>

      <div v-else-if="store.nodes.length === 0" class="text-center text-medium-emphasis pa-4 pa-md-6 pa-lg-8">
        <v-icon size="64" color="grey-lighten-1" class="mb-2">mdi-lan-disconnect</v-icon>
        <p class="mb-2">No machines on the overlay network yet.</p>
        <p class="text-body-2">
          Enroll a device (Devices → Enroll) or a tunnel client and enable the
          overlay on it — each joined machine appears here with its private
          overlay address.
        </p>
      </div>

      <v-table v-else density="compact">
        <thead>
          <tr>
            <th>Machine</th>
            <th>Overlay address</th>
            <th>Kind</th>
            <th>Routes</th>
            <th>Last seen</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="n in store.nodes" :key="n.id">
            <td>
              <div class="d-flex align-center">
                <v-icon size="10" :color="n.online ? 'success' : 'grey'" class="mr-2">mdi-circle</v-icon>
                <span class="font-weight-medium">{{ n.name }}</span>
                <v-chip v-if="n.is_exit_node" size="x-small" color="primary" variant="tonal" label class="ml-2">
                  exit node
                </v-chip>
              </div>
            </td>
            <td class="text-mono">
              <div>{{ n.overlay_ip || '—' }}</div>
              <div v-if="deriveOverlayV6(n.overlay_ip)" class="text-caption text-medium-emphasis">
                {{ deriveOverlayV6(n.overlay_ip) }}
              </div>
            </td>
            <td>
              <v-chip size="x-small" variant="tonal" label>
                {{ n.kind === 'agent' ? 'Device' : 'Tunnel client' }}
              </v-chip>
            </td>
            <td class="text-caption">
              <template v-if="n.approved_routes.length > 0">
                {{ n.approved_routes.length }} approved
              </template>
              <template v-else-if="n.advertised_routes.length > 0">
                <span class="text-medium-emphasis">{{ n.advertised_routes.length }} advertised</span>
              </template>
              <template v-else>—</template>
            </td>
            <td class="text-caption text-medium-emphasis">{{ formatLastSeen(n.last_seen_at) }}</td>
          </tr>
        </tbody>
      </v-table>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { onMounted } from 'vue'
import { useOverlayRoutesStore, deriveOverlayV6 } from '@/stores/overlayRoutes'

const props = defineProps<{ tenantId: string }>()
const store = useOverlayRoutesStore()

function formatLastSeen(iso: string): string {
  if (!iso) return '—'
  try {
    return new Date(iso).toLocaleString()
  } catch {
    return iso
  }
}

onMounted(() => {
  store.fetchNodes(props.tenantId)
})
</script>

<style scoped>
.text-mono {
  font-family: monospace;
}
</style>
