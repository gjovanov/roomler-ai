<template>
  <v-card>
    <v-card-title class="d-flex align-center">
      <span>Subnet Routes</span>
      <v-spacer />
      <v-btn
        prepend-icon="mdi-refresh"
        variant="text"
        size="small"
        :loading="store.loading"
        @click="refresh"
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
        @click:close="store.error = null"
        class="mb-4"
      >
        {{ store.error }}
      </v-alert>

      <p class="text-medium-emphasis mb-4">
        A node advertises subnet CIDRs it can route (set
        <code>overlay_advertised_routes</code> in its agent config). Approve a
        route to distribute it to every other node in the mesh — they'll route
        that LAN through this node. Nothing is routed until you approve it.
      </p>

      <div
        v-if="store.loading && store.nodes.length === 0"
        class="d-flex justify-center pa-8"
      >
        <v-progress-circular indeterminate />
      </div>

      <p
        v-else-if="advertisingNodes.length === 0"
        class="text-medium-emphasis"
      >
        No node is advertising subnet routes yet.
      </p>

      <v-table v-else density="compact">
        <thead>
          <tr>
            <th>Node</th>
            <th>Overlay IP</th>
            <th>Advertised routes</th>
            <th class="text-right">Actions</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="n in advertisingNodes" :key="n.id">
            <td>
              <div class="font-weight-medium">
                {{ n.name || '(unnamed)' }}
                <v-chip
                  size="x-small"
                  :color="n.online ? 'success' : undefined"
                  variant="tonal"
                  class="ml-1"
                >
                  {{ n.online ? 'online' : 'offline' }}
                </v-chip>
              </div>
              <div class="text-caption text-medium-emphasis">{{ n.kind }}</div>
            </td>
            <td>
              <div>{{ n.overlay_ip }}</div>
              <div class="text-caption text-medium-emphasis">
                {{ deriveOverlayV6(n.overlay_ip) }}
              </div>
            </td>
            <td>
              <v-checkbox
                v-for="cidr in n.advertised_routes"
                :key="cidr"
                :label="cidr"
                :model-value="draft[n.id]?.has(cidr) ?? false"
                density="compact"
                hide-details
                @update:model-value="(v) => toggle(n.id, cidr, v)"
              />
            </td>
            <td class="text-right">
              <v-btn
                color="primary"
                variant="flat"
                size="small"
                :loading="saving === n.id"
                :disabled="!dirty(n)"
                @click="save(n)"
              >
                Save
              </v-btn>
            </td>
          </tr>
        </tbody>
      </v-table>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue'
import {
  deriveOverlayV6,
  useOverlayRoutesStore,
  type OverlayNode,
} from '@/stores/overlayRoutes'

const props = defineProps<{ tenantId: string }>()
const store = useOverlayRoutesStore()

// nodeId → editable Set of approved CIDRs (seeded from the server state).
const draft = reactive<Record<string, Set<string>>>({})
const saving = ref<string | null>(null)

const advertisingNodes = computed(() =>
  store.nodes.filter((n) => n.advertised_routes.length > 0),
)

function seedDraft() {
  for (const n of store.nodes) {
    draft[n.id] = new Set(n.approved_routes)
  }
}

watch(() => store.nodes, seedDraft, { immediate: true })

function toggle(nodeId: string, cidr: string, checked: boolean | null) {
  const set = draft[nodeId] ?? (draft[nodeId] = new Set<string>())
  if (checked) set.add(cidr)
  else set.delete(cidr)
}

function dirty(n: OverlayNode): boolean {
  const set = draft[n.id] ?? new Set<string>()
  if (set.size !== n.approved_routes.length) return true
  return n.approved_routes.some((r) => !set.has(r))
}

async function save(n: OverlayNode) {
  saving.value = n.id
  try {
    await store.setApprovedRoutes(
      props.tenantId,
      n.id,
      Array.from(draft[n.id] ?? []),
    )
  } finally {
    saving.value = null
  }
}

async function refresh() {
  await store.fetchNodes(props.tenantId)
}

refresh()
</script>
