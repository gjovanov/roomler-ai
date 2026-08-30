<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <div class="d-flex align-center flex-wrap mb-2 mb-md-4" style="gap: 12px">
      <div>
        <h1 class="text-h5 text-md-h4">Analytics</h1>
        <p class="text-subtitle-2 text-medium-emphasis mb-0">
          Machines, calls and tunnel traffic over time
        </p>
      </div>
      <v-spacer />
      <range-picker v-if="allowed" v-model="range" />
    </div>

    <!-- Fail-closed: never fire a query this membership can't make — the
         API answers 404 and the api client would log the user out on a
         403-class mistake. -->
    <v-alert v-if="!allowed" type="info" variant="tonal" class="mt-4">
      Analytics queries require an org admin role (device management permission).
    </v-alert>

    <template v-else>
      <v-tabs v-model="tab" class="mb-3">
        <v-tab value="machines">Machines</v-tab>
        <v-tab value="calls">Calls</v-tab>
        <v-tab value="tunnels">Tunnels</v-tab>
        <v-tab value="usage">People</v-tab>
      </v-tabs>

      <v-window v-model="tab">
        <v-window-item value="machines">
          <v-row>
            <v-col cols="12" md="6">
              <v-card>
                <v-card-title class="text-subtitle-1">Machines online</v-card-title>
                <v-card-text>
                  <time-series-chart
                    :points="machines?.series ?? []"
                    :series="[{ key: 'online', label: 'Online' }]"
                    area
                  />
                </v-card-text>
              </v-card>
            </v-col>
            <v-col cols="12" md="6">
              <v-card>
                <v-card-title class="text-subtitle-1">
                  CPU / RAM
                  <span class="text-caption text-medium-emphasis ml-2"
                    >needs agents with telemetry v2</span
                  >
                </v-card-title>
                <v-card-text>
                  <time-series-chart
                    :points="machines?.series ?? []"
                    :series="[
                      { key: 'cpu_pct', label: 'CPU %' },
                      { key: 'rss_mb', label: 'RSS MB' },
                    ]"
                    empty-text="No telemetry yet — agents report cpu/ram after the next agent release"
                  />
                </v-card-text>
              </v-card>
            </v-col>
            <v-col cols="12" md="6">
              <v-card>
                <v-card-title class="text-subtitle-1">Transports (direct / relay / derp)</v-card-title>
                <v-card-text>
                  <time-series-chart
                    :points="machines?.series ?? []"
                    :series="[
                      { key: 'direct', label: 'Direct' },
                      { key: 'relay', label: 'Relay' },
                      { key: 'derp', label: 'DERP' },
                    ]"
                    stacked
                    empty-text="No transport telemetry yet — arrives with agent telemetry v2"
                  />
                </v-card-text>
              </v-card>
            </v-col>
            <v-col cols="12" md="6">
              <v-card>
                <v-card-title class="text-subtitle-1">
                  Relayed share
                  <span class="text-caption text-medium-emphasis ml-1">last hour</span>
                </v-card-title>
                <v-card-text>
                  <div class="text-h4 mb-1">
                    <template v-if="relayedPct !== null">{{ relayedPct }}%</template>
                    <span v-else class="text-h6 text-medium-emphasis">no reporters</span>
                  </div>
                  <!--
                    Framed as something to ACT on, not as a bill. A high share
                    means this org's network is refusing direct paths, which
                    their own IT can usually fix; that is why the copy names
                    the likely cause instead of just stating a number.
                  -->
                  <p class="text-caption text-medium-emphasis mb-2">
                    Share of this org's peer links that could not connect
                    directly and fell back to a relay.
                    <strong>Connections, not bytes.</strong>
                    A rising share usually means firewall or NAT policy is
                    blocking direct paths — relayed links are slower and
                    higher-latency than direct ones.
                  </p>
                  <div class="text-caption">
                    <span class="mr-3">Direct: {{ mix?.direct ?? 0 }}</span>
                    <span class="mr-3">Relay: {{ mix?.relay ?? 0 }}</span>
                    <span>DERP: {{ mix?.derp ?? 0 }}</span>
                  </div>
                </v-card-text>
              </v-card>
            </v-col>
            <v-col cols="12" md="6">
              <v-card>
                <v-card-title class="text-subtitle-1">Resources used</v-card-title>
                <v-card-text>
                  <v-table density="compact">
                    <tbody>
                      <tr>
                        <td>Relayed traffic</td>
                        <td class="text-right">{{ relayedBytes }}</td>
                      </tr>
                      <tr>
                        <td>Conference participant-time</td>
                        <td class="text-right">{{ sfuHours }}</td>
                      </tr>
                      <tr>
                        <td>
                          TURN relayed
                          <span class="text-caption text-medium-emphasis">
                            not measured
                          </span>
                        </td>
                        <td class="text-right text-medium-emphasis">—</td>
                      </tr>
                      <tr>
                        <td>Files stored</td>
                        <td class="text-right">{{ storedBytes }}</td>
                      </tr>
                    </tbody>
                  </v-table>
                  <p class="text-caption text-medium-emphasis mt-2 mb-0">
                    Relayed traffic and participant-time are measured over
                    {{ range }} on our own relays; storage is what the org holds
                    right now. Traffic that connected directly is never
                    measured, so it is not counted here.
                  </p>
                </v-card-text>
              </v-card>
            </v-col>
            <v-col cols="12" md="6">
              <v-card>
                <v-card-title class="text-subtitle-1">Peer latency</v-card-title>
                <v-card-text>
                  <time-series-chart
                    :points="machines?.series ?? []"
                    :series="[{ key: 'peer_rtt_ms', label: 'RTT ms' }]"
                    empty-text="No latency telemetry yet — arrives with agent telemetry v2"
                  />
                </v-card-text>
              </v-card>
            </v-col>
            <v-col cols="12" md="6">
              <v-card>
                <v-card-title class="text-subtitle-1">
                  Mesh &amp; tunnel traffic
                  <span class="text-caption text-medium-emphasis ml-2">
                    bytes moved per bucket
                  </span>
                </v-card-title>
                <v-card-text>
                  <time-series-chart
                    :points="machines?.volume ?? []"
                    :series="[
                      { key: 'overlay_rx', label: 'Overlay in' },
                      { key: 'overlay_tx', label: 'Overlay out' },
                      { key: 'tunnel_rx', label: 'Tunnel in' },
                      { key: 'tunnel_tx', label: 'Tunnel out' },
                    ]"
                    :y-format="fmtBytes"
                    stacked
                    empty-text="No traffic telemetry yet — arrives with agent telemetry v3"
                  />
                </v-card-text>
              </v-card>
            </v-col>
            <v-col cols="12">
              <v-card>
                <v-card-title class="text-subtitle-1">
                  Device uptime
                  <span class="text-caption text-medium-emphasis ml-2">
                    presence over the selected range
                  </span>
                </v-card-title>
                <v-card-text>
                  <uptime-strip
                    :agents="machines?.uptime ?? []"
                    empty-text="No presence transitions recorded in this range"
                  />
                </v-card-text>
              </v-card>
            </v-col>
          </v-row>
        </v-window-item>

        <v-window-item value="calls">
          <v-row>
            <v-col cols="12" md="4">
              <v-card>
                <v-card-text class="d-flex justify-space-around text-center">
                  <div>
                    <div class="text-h5">{{ Math.round(calls?.totals?.calls ?? 0) }}</div>
                    <div class="text-caption text-medium-emphasis">Calls</div>
                  </div>
                  <div>
                    <div class="text-h5">{{ Math.round(calls?.totals?.minutes ?? 0) }}</div>
                    <div class="text-caption text-medium-emphasis">Minutes</div>
                  </div>
                  <div>
                    <div class="text-h5">
                      {{ Math.round(calls?.totals?.participant_minutes ?? 0) }}
                    </div>
                    <div class="text-caption text-medium-emphasis">Participant-minutes</div>
                  </div>
                </v-card-text>
              </v-card>
              <v-card class="mt-4">
                <v-card-title class="text-subtitle-1">Peak participants</v-card-title>
                <v-card-text>
                  <time-series-chart
                    :points="calls?.series ?? []"
                    :series="[{ key: 'peak_participants', label: 'Peak' }]"
                    :height="140"
                  />
                </v-card-text>
              </v-card>
            </v-col>
            <v-col cols="12" md="8">
              <v-card>
                <v-card-title class="text-subtitle-1">Call minutes</v-card-title>
                <v-card-text>
                  <time-series-chart
                    :points="callMinutesSeries"
                    :series="[
                      { key: 'call_minutes', label: 'Call minutes' },
                      { key: 'participant_minutes', label: 'Participant-minutes' },
                    ]"
                    area
                  />
                </v-card-text>
              </v-card>
              <v-card class="mt-4">
                <v-card-title class="text-subtitle-1">Relayed vs direct participants</v-card-title>
                <v-card-text>
                  <time-series-chart
                    :points="callMinutesSeries"
                    :series="[
                      { key: 'direct_minutes', label: 'Direct' },
                      { key: 'relayed_minutes', label: 'Relayed' },
                    ]"
                    stacked
                  />
                </v-card-text>
              </v-card>
            </v-col>
          </v-row>
        </v-window-item>

        <v-window-item value="tunnels">
          <v-row>
            <v-col cols="12" md="6">
              <v-card>
                <v-card-title class="text-subtitle-1">Tunnel traffic</v-card-title>
                <v-card-text>
                  <time-series-chart
                    :points="tunnels?.series ?? []"
                    :series="[
                      { key: 'bytes_in', label: 'Bytes in' },
                      { key: 'bytes_out', label: 'Bytes out' },
                    ]"
                    area
                    :y-format="fmtBytes"
                  />
                </v-card-text>
              </v-card>
            </v-col>
            <v-col cols="12" md="6">
              <v-card>
                <v-card-title class="text-subtitle-1">Flows: direct vs relayed</v-card-title>
                <v-card-text>
                  <time-series-chart
                    :points="tunnels?.series ?? []"
                    :series="[
                      { key: 'direct', label: 'Direct' },
                      { key: 'relayed', label: 'Relayed' },
                    ]"
                    stacked
                  />
                </v-card-text>
              </v-card>
            </v-col>
          </v-row>
        </v-window-item>

        <v-window-item value="usage">
          <v-card>
            <v-card-title class="text-subtitle-1">Usage by person</v-card-title>
            <v-card-subtitle>
              Minutes and traffic per member. Select someone to see when they viewed which
              machine.
            </v-card-subtitle>
            <v-card-text>
              <usage-panel scope="org" :tenant-id="props.tenantId" :range="range" />
            </v-card-text>
          </v-card>
        </v-window-item>
      </v-window>
    </template>
  </v-container>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useTenantStore } from '@/stores/tenant'
import {
  useStatsStore,
  type ResourcesPayload,
  type SeriesPayload,
  type SeriesPoint,
} from '@/stores/stats'
import { canQueryAnalytics } from '@/utils/permissions'
import TimeSeriesChart from '@/components/stats/TimeSeriesChart.vue'
import UptimeStrip from '@/components/stats/UptimeStrip.vue'
import UsagePanel from '@/components/stats/UsagePanel.vue'
import RangePicker, { type StatsRange } from '@/components/stats/RangePicker.vue'

const props = defineProps<{ tenantId: string }>()

const tenantStore = useTenantStore()
const statsStore = useStatsStore()

const allowed = computed(() =>
  canQueryAnalytics(tenantStore.myPermissions, tenantStore.isOwner),
)

const tab = ref<'machines' | 'calls' | 'tunnels' | 'usage'>('machines')
const range = ref<StatsRange>('7d')
const machines = ref<SeriesPayload | null>(null)
const calls = ref<SeriesPayload | null>(null)
const tunnels = ref<SeriesPayload | null>(null)
const resources = ref<ResourcesPayload | null>(null)

// FR-20 P6. Every getter below passes `null` through rather than coercing it:
// a relayed share of 0% claims a flawless mesh, and an unmonitored meter shown
// as 0 claims we measured something we did not.
const mix = computed(() => resources.value?.carrier_mix ?? null)
const relayedPct = computed<number | null>(() => {
  const f = mix.value?.relayed_fraction
  return f === null || f === undefined ? null : Math.round(f * 1000) / 10
})
const relayedBytes = computed(() => {
  const m = resources.value?.meters?.derp_bytes
  return m && m.monitored && m.total !== null ? fmtBytes(m.total) : '—'
})
const sfuHours = computed(() => {
  const m = resources.value?.meters?.sfu_participant_seconds
  if (!m || !m.monitored || m.total === null) return '—'
  return `${(m.total / 3600).toFixed(1)} h`
})
const storedBytes = computed(() => {
  const b = resources.value?.storage?.bytes
  return typeof b === 'number' ? fmtBytes(b) : '—'
})

const callMinutesSeries = computed<SeriesPoint[]>(() =>
  (calls.value?.series ?? []).map((p) => ({
    t: p.t,
    call_minutes: num(p.call_seconds) / 60,
    participant_minutes: num(p.participant_seconds) / 60,
    relayed_minutes: num(p.relayed_seconds) / 60,
    direct_minutes: num(p.direct_seconds) / 60,
  })),
)
function num(v: number | null | undefined): number {
  return typeof v === 'number' && Number.isFinite(v) ? v : 0
}
function fmtBytes(v: number): string {
  if (v >= 1_073_741_824) return `${(v / 1_073_741_824).toFixed(1)} GiB`
  if (v >= 1_048_576) return `${(v / 1_048_576).toFixed(1)} MiB`
  if (v >= 1024) return `${(v / 1024).toFixed(1)} KiB`
  return `${Math.round(v)} B`
}

async function load() {
  if (!allowed.value || !props.tenantId) return
  const [m, c, t, r] = await Promise.all([
    statsStore.fetchMachines(props.tenantId, range.value).catch(() => null),
    statsStore.fetchCalls(props.tenantId, range.value).catch(() => null),
    statsStore.fetchTunnels(props.tenantId, range.value).catch(() => null),
    statsStore.fetchResources(props.tenantId, range.value).catch(() => null),
  ])
  machines.value = m
  calls.value = c
  tunnels.value = t
  resources.value = r
}

watch([() => props.tenantId, range, allowed], load, { immediate: true })
</script>
