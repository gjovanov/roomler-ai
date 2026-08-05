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
      </v-window>
    </template>
  </v-container>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useTenantStore } from '@/stores/tenant'
import { useStatsStore, type SeriesPayload, type SeriesPoint } from '@/stores/stats'
import { canQueryAnalytics } from '@/utils/permissions'
import TimeSeriesChart from '@/components/stats/TimeSeriesChart.vue'
import RangePicker, { type StatsRange } from '@/components/stats/RangePicker.vue'

const props = defineProps<{ tenantId: string }>()

const tenantStore = useTenantStore()
const statsStore = useStatsStore()

const allowed = computed(() =>
  canQueryAnalytics(tenantStore.myPermissions, tenantStore.isOwner),
)

const tab = ref<'machines' | 'calls' | 'tunnels'>('machines')
const range = ref<StatsRange>('7d')
const machines = ref<SeriesPayload | null>(null)
const calls = ref<SeriesPayload | null>(null)
const tunnels = ref<SeriesPayload | null>(null)

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
  const [m, c, t] = await Promise.all([
    statsStore.fetchMachines(props.tenantId, range.value).catch(() => null),
    statsStore.fetchCalls(props.tenantId, range.value).catch(() => null),
    statsStore.fetchTunnels(props.tenantId, range.value).catch(() => null),
  ])
  machines.value = m
  calls.value = c
  tunnels.value = t
}

watch([() => props.tenantId, range, allowed], load, { immediate: true })
</script>
