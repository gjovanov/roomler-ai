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
            <!-- Actions leftmost: a right-hand actions column falls off narrow
                 viewports (same fix as AgentsSection). -->
            <th class="text-center" style="width: 56px"></th>
            <th>Machine</th>
            <th>Overlay address</th>
            <th>Kind</th>
            <th>Routes</th>
            <th>Last seen</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="n in store.nodes" :key="n.id">
            <td class="text-center">
              <v-btn
                icon="mdi-lan-disconnect"
                color="error"
                size="small"
                variant="text"
                :aria-label="`Remove ${n.name} from the mesh`"
                @click="confirmEvict(n)"
              />
            </td>
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

    <v-dialog v-model="evictDialogOpen" max-width="540">
      <v-card>
        <v-card-title>
          {{ evictTarget?.will_rejoin
            ? `Force a new overlay address for “${evictTarget?.name}”?`
            : `Remove “${evictTarget?.name}” from the mesh?` }}
        </v-card-title>
        <v-card-text>
          <ul class="text-body-2 mb-3 ps-4">
            <li>
              It leaves the mesh immediately — every peer drops its routes, and
              traffic to
              <span class="text-mono">{{ evictTarget?.overlay_ip }}</span>
              stops.
            </li>
            <li>
              That address is released back to this tenant's pool and may later
              be assigned to a <strong>different</strong> machine.
            </li>
            <li v-if="evictTarget?.will_rejoin">
              <strong>{{ evictTarget?.name }}</strong> is still enrolled, so it
              rejoins automatically on its next connect — with a
              <strong>different</strong> overlay address. This does not revoke
              access; to remove the device for good, delete the agent or tunnel
              client.
            </li>
            <li v-else>
              Its backing device is no longer enrolled, so it will not come back.
            </li>
          </ul>
          <v-alert type="warning" variant="tonal" density="compact" class="mb-0">
            Anything pinned to the old address stops working — firewall rules,
            scripts, and any client configured with
            <code>overlay_exit_node = "{{ evictTarget?.name }}"</code>.
          </v-alert>
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="evictDialogOpen = false">Cancel</v-btn>
          <v-btn color="error" variant="flat" :loading="evicting" @click="performEvict">
            {{ evictTarget?.will_rejoin ? 'Evict and reassign' : 'Remove from mesh' }}
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </v-card>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useOverlayRoutesStore, deriveOverlayV6, type OverlayNode } from '@/stores/overlayRoutes'
import { useSnackbar } from '@/composables/useSnackbar'

const props = defineProps<{ tenantId: string }>()
const store = useOverlayRoutesStore()
const { showSuccess } = useSnackbar()

const evictDialogOpen = ref(false)
const evictTarget = ref<OverlayNode | null>(null)
const evicting = ref(false)

function confirmEvict(n: OverlayNode) {
  evictTarget.value = n
  evictDialogOpen.value = true
}

async function performEvict() {
  const target = evictTarget.value
  if (!target) return
  evicting.value = true
  try {
    const res = await store.evictNode(props.tenantId, target.id)
    showSuccess(
      res.host_recycled
        ? `${res.name} left the mesh — ${res.overlay_ip} is back in the pool.`
        : `${res.name} left the mesh.`,
    )
    evictDialogOpen.value = false
  } catch (e) {
    store.error = (e as Error).message
  } finally {
    evicting.value = false
  }
}

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
