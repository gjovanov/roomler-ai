<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <div class="d-flex align-center flex-wrap mb-2 mb-md-4" style="gap: 12px">
      <div>
        <h1 class="text-h5 text-md-h4">Observability</h1>
        <p class="text-subtitle-2 text-medium-emphasis mb-0">
          Relay cluster, orgs and calls — platform view
        </p>
      </div>
      <v-spacer />
      <range-picker v-if="isPlatformAdmin" v-model="range" />
    </div>

    <v-alert v-if="!isPlatformAdmin" type="info" variant="tonal" class="mt-4">
      This page is limited to platform operators.
    </v-alert>

    <template v-else>
      <!-- ── Relay fleet ─────────────────────────────────────────────── -->
      <h2 class="text-h6 mb-2">Relay fleet (coturn + DERP)</h2>
      <v-row dense>
        <v-col
          v-for="r in regionCards"
          :key="r.id"
          cols="12"
          sm="6"
          md="4"
          lg="3"
        >
          <v-card
            :variant="r.id === selectedRegion ? 'tonal' : 'elevated'"
            hover
            @click="selectedRegion = r.id"
          >
            <v-card-text>
              <div class="d-flex align-center">
                <span class="text-subtitle-1 font-weight-medium">{{ r.id }}</span>
                <v-spacer />
                <v-chip
                  v-if="!r.monitored"
                  size="x-small"
                  color="grey"
                  variant="tonal"
                >not monitored</v-chip>
                <v-chip
                  v-else-if="r.healthy === false"
                  size="x-small"
                  color="error"
                >down</v-chip>
                <v-chip v-else-if="r.busy" size="x-small" color="warning">busy</v-chip>
                <v-chip v-else size="x-small" color="success" variant="tonal">ok</v-chip>
              </div>
              <div class="text-caption text-medium-emphasis mt-1" style="min-height: 40px">
                <template v-if="r.monitored && r.latest">
                  load {{ fmt(r.latest.load1) }} · tx {{ fmt(r.latest.tx_mbps) }} Mbps · rx
                  {{ fmt(r.latest.rx_mbps) }} Mbps<br />
                  mem free {{ fmtPct(r.latest.mem_available_pct) }} · allocs
                  {{ fmt(r.latest.allocations) }} · derp {{ fmt(r.latest.derp_registrations) }}
                  <br />
                  poll rtt {{ fmt(r.latest.poll_rtt_ms) }} ms
                  <template v-if="r.agentRtt"> · fleet rtt {{ fmt(r.agentRtt) }} ms</template>
                </template>
                <template v-else-if="!r.monitored">
                  no /stats endpoint (central fleet) — TURN still served
                </template>
                <template v-else>no samples yet</template>
              </div>
            </v-card-text>
          </v-card>
        </v-col>
      </v-row>

      <v-row v-if="selectedRegion" class="mt-1">
        <v-col cols="12" md="6">
          <v-card>
            <v-card-title class="text-subtitle-1">
              {{ selectedRegion }} — availability &amp; latency
            </v-card-title>
            <v-card-text>
              <time-series-chart
                :points="history?.series ?? []"
                :series="[
                  { key: 'healthy_pct', label: 'Healthy %' },
                  { key: 'poll_rtt_ms', label: 'Poll RTT ms' },
                ]"
              />
            </v-card-text>
          </v-card>
        </v-col>
        <v-col cols="12" md="6">
          <v-card>
            <v-card-title class="text-subtitle-1">
              {{ selectedRegion }} — load &amp; traffic
            </v-card-title>
            <v-card-text>
              <time-series-chart
                :points="history?.series ?? []"
                :series="[
                  { key: 'load1', label: 'load1' },
                  { key: 'tx_mbps', label: 'TX Mbps' },
                  { key: 'rx_mbps', label: 'RX Mbps' },
                  { key: 'allocations', label: 'TURN allocs' },
                ]"
              />
            </v-card-text>
          </v-card>
        </v-col>
      </v-row>

      <!-- ── Orgs ────────────────────────────────────────────────────── -->
      <h2 class="text-h6 mt-6 mb-2">Organizations</h2>
      <v-card>
        <v-table density="compact">
          <thead>
            <tr>
              <th>Org</th>
              <th class="text-right">Machines online</th>
              <th class="text-right">Calls (30d)</th>
              <th class="text-right">Minutes (30d)</th>
              <th />
            </tr>
          </thead>
          <tbody>
            <tr v-for="o in orgRows" :key="o.id">
              <td>{{ o.name }}</td>
              <td class="text-right">{{ o.online }} / {{ o.total }}</td>
              <td class="text-right">{{ o.calls }}</td>
              <td class="text-right">{{ Math.round(o.minutes) }}</td>
              <td class="text-right">
                <v-btn
                  size="x-small"
                  variant="tonal"
                  @click="selectOrg(o.id)"
                >inspect</v-btn>
              </td>
            </tr>
            <tr v-if="orgRows.length === 0">
              <td colspan="5" class="text-medium-emphasis">No organizations yet</td>
            </tr>
          </tbody>
        </v-table>
      </v-card>

      <v-row v-if="selectedOrg" class="mt-1">
        <v-col cols="12" md="6">
          <v-card>
            <v-card-title class="text-subtitle-1">
              {{ orgName(selectedOrg) }} — machines online
            </v-card-title>
            <v-card-text>
              <time-series-chart
                :points="orgMachines?.series ?? []"
                :series="[{ key: 'online', label: 'Online' }]"
                area
              />
            </v-card-text>
          </v-card>
        </v-col>
        <v-col cols="12" md="6">
          <v-card>
            <v-card-title class="text-subtitle-1">
              {{ orgName(selectedOrg) }} — participant-minutes
            </v-card-title>
            <v-card-text>
              <time-series-chart
                :points="orgCallMinutes"
                :series="[{ key: 'participant_minutes', label: 'Participant-minutes' }]"
                area
              />
            </v-card-text>
          </v-card>
        </v-col>
      </v-row>

      <!-- ── Calls (platform-wide) ───────────────────────────────────── -->
      <h2 class="text-h6 mt-6 mb-2">Calls — platform wide</h2>
      <v-row>
        <v-col cols="12" md="8">
          <v-card>
            <v-card-title class="text-subtitle-1">Participant-minutes</v-card-title>
            <v-card-text>
              <time-series-chart
                :points="globalCallMinutes"
                :series="[
                  { key: 'participant_minutes', label: 'Participant-minutes' },
                  { key: 'relayed_minutes', label: 'Relayed' },
                ]"
                area
              />
            </v-card-text>
          </v-card>
        </v-col>
        <v-col cols="12" md="4">
          <v-card>
            <v-card-text class="d-flex justify-space-around text-center">
              <div>
                <div class="text-h5">{{ Math.round(globalCalls?.totals?.calls ?? 0) }}</div>
                <div class="text-caption text-medium-emphasis">Calls</div>
              </div>
              <div>
                <div class="text-h5">{{ Math.round(globalCalls?.totals?.minutes ?? 0) }}</div>
                <div class="text-caption text-medium-emphasis">Minutes</div>
              </div>
            </v-card-text>
          </v-card>
        </v-col>
      </v-row>
    </template>
  </v-container>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useAuthStore } from '@/stores/auth'
import {
  useStatsStore,
  type OrgsPayload,
  type SeriesPayload,
  type SeriesPoint,
} from '@/stores/stats'
import { usePolling } from '@/composables/usePolling'
import TimeSeriesChart from '@/components/stats/TimeSeriesChart.vue'
import RangePicker, { type StatsRange } from '@/components/stats/RangePicker.vue'

const authStore = useAuthStore()
const statsStore = useStatsStore()

const isPlatformAdmin = computed(() => authStore.user?.is_platform_admin === true)

const range = ref<StatsRange>('24h')
const selectedRegion = ref<string | null>(null)
const selectedOrg = ref<string | null>(null)
const history = ref<SeriesPayload | null>(null)
const orgs = ref<OrgsPayload | null>(null)
const orgMachines = ref<SeriesPayload | null>(null)
const orgCalls = ref<SeriesPayload | null>(null)
const globalCalls = ref<SeriesPayload | null>(null)

// Realtime: 15 s poll of the current snapshot (reads the newest persisted
// buckets — cheap), paused while the tab is hidden.
usePolling(async () => {
  if (!isPlatformAdmin.value) return
  await statsStore.fetchRelayCurrent()
  if (!selectedRegion.value) {
    const first = regionCards.value.find((r) => r.monitored)
    if (first) selectedRegion.value = first.id
  }
}, 15_000)

interface RegionCard {
  id: string
  enabled: boolean
  monitored: boolean
  busy: boolean
  healthy?: boolean
  latest?: SeriesPoint & { healthy?: boolean }
  agentRtt?: number
}
const regionCards = computed<RegionCard[]>(() => {
  const cur = statsStore.relayCurrent
  if (!cur?.regions) return []
  const latestBy = new Map((cur.latest ?? []).map((l) => [String(l.region), l]))
  const rttBy = new Map((cur.agent_rtt ?? []).map((r) => [r.region, r.rtt_avg_ms]))
  return cur.regions.map((r) => {
    const latest = latestBy.get(r.id)
    return {
      id: r.id,
      enabled: r.enabled,
      monitored: r.monitored,
      busy: r.busy,
      healthy: latest?.healthy,
      latest,
      agentRtt: rttBy.get(r.id),
    }
  })
})

watch(
  [selectedRegion, range, isPlatformAdmin],
  async () => {
    if (!isPlatformAdmin.value || !selectedRegion.value) return
    history.value = await statsStore
      .fetchRelayHistory(selectedRegion.value, range.value)
      .catch(() => null)
  },
  { immediate: true },
)

watch(
  [isPlatformAdmin, range],
  async () => {
    if (!isPlatformAdmin.value) return
    orgs.value = await statsStore.fetchOrgs().catch(() => null)
    globalCalls.value = await statsStore.fetchAdminCalls(range.value).catch(() => null)
    if (selectedOrg.value) await selectOrg(selectedOrg.value)
  },
  { immediate: true },
)

interface OrgRow {
  id: string
  name: string
  online: number
  total: number
  calls: number
  minutes: number
}
const orgRows = computed<OrgRow[]>(() => {
  const o = orgs.value
  if (!o?.tenants) return []
  const m = new Map((o.machines ?? []).map((x) => [x.tenant_id, x]))
  const c = new Map((o.calls ?? []).map((x) => [x.tenant_id, x]))
  return o.tenants.map((t) => ({
    id: t.id,
    name: t.name,
    online: m.get(t.id)?.online ?? 0,
    total: m.get(t.id)?.total ?? 0,
    calls: c.get(t.id)?.calls_30d ?? 0,
    minutes: c.get(t.id)?.minutes_30d ?? 0,
  }))
})
function orgName(id: string): string {
  return orgRows.value.find((o) => o.id === id)?.name ?? id
}
async function selectOrg(id: string) {
  selectedOrg.value = id
  const [m, c] = await Promise.all([
    statsStore.fetchAdminMachines(id, range.value).catch(() => null),
    statsStore.fetchAdminCalls(range.value, id).catch(() => null),
  ])
  orgMachines.value = m
  orgCalls.value = c
}

function toMinutes(series: SeriesPoint[] | undefined): SeriesPoint[] {
  return (series ?? []).map((p) => ({
    t: p.t,
    participant_minutes: num(p.participant_seconds) / 60,
    relayed_minutes: num(p.relayed_seconds) / 60,
  }))
}
const orgCallMinutes = computed(() => toMinutes(orgCalls.value?.series))
const globalCallMinutes = computed(() => toMinutes(globalCalls.value?.series))

function num(v: number | null | undefined): number {
  return typeof v === 'number' && Number.isFinite(v) ? v : 0
}
function fmt(v: number | null | undefined): string {
  return typeof v === 'number' && Number.isFinite(v) ? `${Math.round(v * 10) / 10}` : '—'
}
function fmtPct(v: number | null | undefined): string {
  return typeof v === 'number' && Number.isFinite(v) ? `${Math.round(v * 100)}%` : '—'
}
</script>
