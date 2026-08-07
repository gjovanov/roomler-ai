<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <h1 class="text-h5 text-md-h4 mb-1 mb-md-2">{{ tenantStore.current?.name }}</h1>
    <p class="text-subtitle-2 text-md-subtitle-1 text-medium-emphasis mb-2 mb-md-4">Workspace Overview</p>

    <v-row>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/rooms`" hover>
          <v-card-text class="text-center">
            <v-icon size="48" color="primary">mdi-pound</v-icon>
            <div class="text-h4 mt-2">{{ roomStore.rooms.length }}</div>
            <div class="text-subtitle-2">Rooms</div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card>
          <v-card-text class="text-center">
            <v-icon size="48" color="secondary">mdi-video</v-icon>
            <div class="text-h4 mt-2">{{ activeCallCount }}</div>
            <div class="text-subtitle-2">Active Calls</div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/rooms`" hover>
          <v-card-text class="text-center">
            <v-icon size="48" color="accent">mdi-message-text</v-icon>
            <div class="text-h4 mt-2">{{ totalMessageCount }}</div>
            <div class="text-subtitle-2">Messages</div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="showFleet ? `/tenant/${tenantId}/devices` : undefined" :hover="showFleet">
          <v-card-text class="text-center">
            <v-icon size="48" color="warning">mdi-monitor-multiple</v-icon>
            <div class="text-h4 mt-2">{{ onlineDevices }}</div>
            <div class="text-subtitle-2">Devices Online</div>
          </v-card-text>
        </v-card>
      </v-col>
    </v-row>

    <!-- Insights (stats PR-4): the basic org graphs, visible to EVERY
         member — the overview endpoint is member-safe; deep queries live
         behind Analytics (org admins). Hidden entirely while stats are
         disabled server-side. -->
    <template v-if="statsStore.overview?.enabled">
      <h2 class="text-h6 mt-4 mb-2">Insights</h2>
      <v-row>
        <v-col cols="12" md="6">
          <v-card>
            <v-card-title class="text-subtitle-1 d-flex align-center">
              Machines online — 24h
              <v-spacer />
              <span class="text-h6">
                {{ statsStore.overview?.machines?.online ?? 0 }}/{{
                  statsStore.overview?.machines?.total ?? 0
                }}
              </span>
            </v-card-title>
            <v-card-text>
              <time-series-chart
                :points="statsStore.overview?.spark_machines ?? []"
                :series="[{ key: 'online', label: 'Online' }]"
                :height="140"
                area
                empty-text="No samples yet — data appears within the hour"
              />
            </v-card-text>
          </v-card>
        </v-col>
        <v-col cols="12" md="6">
          <v-card>
            <v-card-title class="text-subtitle-1 d-flex align-center">
              Call minutes — 7d
              <v-spacer />
              <span class="text-h6">
                {{ Math.round(statsStore.overview?.calls?.minutes_today ?? 0) }} today
              </span>
            </v-card-title>
            <v-card-text>
              <time-series-chart
                :points="statsStore.overview?.spark_minutes ?? []"
                :series="[{ key: 'minutes', label: 'Minutes' }]"
                :height="140"
                area
                empty-text="No calls in the last 7 days"
              />
            </v-card-text>
          </v-card>
        </v-col>
      </v-row>
    </template>

    <!-- Overlay mesh (wave 2): how this org's devices actually reach the
         control plane and each other. Member-visible — carrier kind and
         latency only, no addresses. -->
    <template v-if="meshNodes.length">
      <h2 class="text-h6 mt-4 mb-2">Network</h2>
      <v-card>
        <v-card-text>
          <mesh-graph
            :nodes="meshNodes"
            :edges="meshEdges"
            :center-name="centerName"
            @select="onMeshSelect"
          />
        </v-card-text>
      </v-card>
    </template>

    <!-- Quick actions -->
    <h2 class="text-h6 mt-4 mb-2">Quick Actions</h2>
    <v-row>
      <v-col cols="12" sm="6" md="3">
        <v-btn block color="primary" prepend-icon="mdi-plus" :to="`/tenant/${tenantId}/rooms`">
          New Room
        </v-btn>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-btn block color="secondary" prepend-icon="mdi-video-plus" @click="startCall">
          Start Call
        </v-btn>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-btn block color="accent" prepend-icon="mdi-upload" :to="`/tenant/${tenantId}/files`">
          Upload File
        </v-btn>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-btn block prepend-icon="mdi-compass" :to="`/tenant/${tenantId}/explore`">
          Explore
        </v-btn>
      </v-col>
    </v-row>

    <!-- Manage: every subsystem reachable from the org hub — the page used
         to dead-end at chat stats with no path to devices/network/admin. -->
    <h2 class="text-h6 mt-4 mb-2">Manage</h2>
    <v-row>
      <v-col v-if="showFleet" cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/devices`" hover>
          <v-card-text>
            <v-icon color="primary" class="mr-2">mdi-monitor-multiple</v-icon>
            <span class="text-subtitle-1 font-weight-medium">Devices</span>
            <div class="text-body-2 text-medium-emphasis mt-1">
              Remote control, enrollment, updates
            </div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col v-if="showFleet" cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/network/machines`" hover>
          <v-card-text>
            <v-icon color="primary" class="mr-2">mdi-lan</v-icon>
            <span class="text-subtitle-1 font-weight-medium">Network</span>
            <div class="text-body-2 text-medium-emphasis mt-1">
              Overlay mesh, tunnels, ACL, routes, DNS
            </div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/admin/members`" hover>
          <v-card-text>
            <v-icon color="primary" class="mr-2">mdi-account-group</v-icon>
            <span class="text-subtitle-1 font-weight-medium">Members &amp; Roles</span>
            <div class="text-body-2 text-medium-emphasis mt-1">
              People, permissions, org settings
            </div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/invites`" hover>
          <v-card-text>
            <v-icon color="primary" class="mr-2">mdi-email-plus</v-icon>
            <span class="text-subtitle-1 font-weight-medium">Invites</span>
            <div class="text-body-2 text-medium-emphasis mt-1">
              Bring teammates into this org
            </div>
          </v-card-text>
        </v-card>
      </v-col>
      <v-col v-if="showAnalytics" cols="12" sm="6" md="3">
        <v-card :to="`/tenant/${tenantId}/analytics`" hover>
          <v-card-text>
            <v-icon color="primary" class="mr-2">mdi-chart-areaspline</v-icon>
            <span class="text-subtitle-1 font-weight-medium">Analytics</span>
            <div class="text-body-2 text-medium-emphasis mt-1">
              Machines, calls, tunnel traffic over time
            </div>
          </v-card-text>
        </v-card>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { computed, onMounted, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useTenantStore } from '@/stores/tenant'
import { useRoomStore } from '@/stores/rooms'
import { useAgentStore } from '@/stores/agents'
import { useStatsStore } from '@/stores/stats'
import { usePolling } from '@/composables/usePolling'
import { canQueryAnalytics, canSeeFleetNav } from '@/utils/permissions'
import TimeSeriesChart from '@/components/stats/TimeSeriesChart.vue'
import MeshGraph, { type MeshNode } from '@/components/stats/MeshGraph.vue'

const route = useRoute()
const router = useRouter()
const tenantStore = useTenantStore()
const roomStore = useRoomStore()
const agentStore = useAgentStore()
const statsStore = useStatsStore()

const tenantId = computed(() => route.params.tenantId as string)
const activeCallCount = computed(
  () => roomStore.rooms.filter((r) => r.conference_status === 'in_progress' && (r.participant_count || 0) > 0).length,
)
const totalMessageCount = computed(
  () => roomStore.rooms.reduce((sum, r) => sum + (r.message_count || 0), 0),
)
const onlineDevices = computed(() => agentStore.agents.filter((a) => a.is_online).length)
const showFleet = computed(() => canSeeFleetNav(tenantStore.myPermissions, tenantStore.isOwner))
const showAnalytics = computed(() =>
  canQueryAnalytics(tenantStore.myPermissions, tenantStore.isOwner),
)

// Insights panel: member-safe overview, refreshed every 60 s (paused
// while hidden). The store swallows 404s so a member of a stats-disabled
// deployment just sees no panel.
usePolling(async () => {
  if (!tenantId.value) return
  await statsStore.fetchOverview(tenantId.value)
  await statsStore.fetchMesh(tenantId.value)
}, 60_000)

// The mesh payload keys edges by OVERLAY node id while presence and
// version live on the agent row — join them here so the graph gets one
// flat node list. A device with no overlay node simply isn't in the mesh.
const meshNodes = computed<MeshNode[]>(() => {
  const m = statsStore.mesh
  if (!m?.nodes) return []
  const agentById = new Map((m.agents ?? []).map((a) => [a.id, a]))
  return m.nodes.map((n) => {
    const agent = n.agent_id_hex ? agentById.get(n.agent_id_hex) : undefined
    return {
      id: n.id,
      name: agent?.name || n.name || n.overlay_ip || n.id.slice(-6),
      online: agent?.last_presence === 'online',
      relay_home: agent?.relay_home ?? n.relay_home ?? null,
      version: agent?.agent_version ?? null,
    }
  })
})
const meshEdges = computed(() => statsStore.mesh?.edges ?? [])
const centerName = computed(() => statsStore.mesh?.center?.name ?? 'roomler.ai')

function onMeshSelect(nodeId: string) {
  // Clicking a device jumps to the fleet page it belongs to — the graph
  // answers "who is reachable how", Devices answers "what do I do about it".
  const agentHex = statsStore.mesh?.nodes?.find((n) => n.id === nodeId)?.agent_id_hex
  if (agentHex && showFleet.value) {
    router.push(`/tenant/${tenantId.value}/devices`)
  }
}
watch(tenantId, (tid) => {
  if (tid) void statsStore.fetchOverview(tid)
})

async function startCall() {
  const now = new Date()
  const name = `Call - ${now.toLocaleDateString()} ${now.toLocaleTimeString()}`
  const room = await roomStore.createRoom(tenantId.value, {
    name,
    has_media: true,
    is_open: true,
  })
  router.push({ name: 'room-call', params: { tenantId: tenantId.value, roomId: room.id } })
}

onMounted(() => {
  if (tenantId.value) {
    roomStore.fetchRooms(tenantId.value)
    if (showFleet.value) agentStore.fetchAgents(tenantId.value).catch(() => {})
  }
})
</script>
