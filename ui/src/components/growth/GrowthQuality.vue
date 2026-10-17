<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- FR-102 P9a — remote-desktop quality from what is already recorded,
       read from the rows the server freezes nightly per day and agent
       version: the sessions requested and what became of them, the
       duration / frame-rate / RTT distributions, FR-22's time to first
       frame (frozen before its 7-day log horizon) and crashes per 100
       device-days. Three cells, never flattened: a value, a "not measured"
       chip with its reason, and a measured zero. A percentile pooled across
       days is an estimate and is marked ≈. Versions and reasons are text,
       never v-html. -->
  <div>
    <div class="d-flex align-center flex-wrap mb-3" style="gap: 12px">
      <v-text-field
        v-model="since"
        type="date"
        label="Since (UTC)"
        density="compact"
        hide-details
        style="max-width: 180px"
        :max="until"
      />
      <v-text-field
        v-model="until"
        type="date"
        label="Until (UTC)"
        density="compact"
        hide-details
        style="max-width: 180px"
        :max="yesterday"
      />
      <v-switch
        v-model="includeInternal"
        label="Include internal"
        density="compact"
        hide-details
        color="warning"
      />
      <v-btn size="small" variant="tonal" :loading="loading" @click="load">Load</v-btn>
    </div>

    <v-alert v-if="message" type="error" variant="tonal" density="compact" class="mb-3">
      {{ message }}
    </v-alert>

    <template v-if="report">
      <div class="text-caption text-medium-emphasis mb-2">
        {{ report.since }} → {{ report.until }} · {{ report.frozen_days }} of {{ report.days }} days frozen ·
        excludes {{ report.excluded.users }} users, {{ report.excluded.tenants }} orgs
        <template v-if="report.include_internal"> · INTERNAL INCLUDED</template>
        · rates from n ≥ {{ report.n_floor }} · time to first frame kept {{ report.ttff_horizon_days }} days in
        agent_logs, frozen nightly before that
      </div>

      <growth-not-measured v-if="report.not_measured" :reason="report.not_measured.not_measured" />

      <template v-else-if="report.totals">
        <v-row dense class="mb-2">
          <v-col v-for="c in cards" :key="c.label" cols="6" sm="4" md="2">
            <v-card variant="tonal" class="fill-height">
              <v-card-text>
                <div class="text-caption text-medium-emphasis">{{ c.label }}</div>
                <div class="text-h6">{{ c.value }}</div>
                <div v-if="c.note" class="text-caption text-medium-emphasis">{{ c.note }}</div>
              </v-card-text>
            </v-card>
          </v-col>
        </v-row>

        <v-card class="mb-3">
          <v-card-title class="text-subtitle-1 d-flex align-center flex-wrap" style="gap: 8px">
            Time to first frame per day
            <span class="text-caption text-medium-emphasis">
              foreground tab · first attempt · no drop (FR-22's record) · p50 and p90, exact per day
            </span>
          </v-card-title>
          <v-card-text>
            <time-series-chart
              v-if="ttffPoints.length"
              :points="ttffPoints"
              :series="[
                { key: 'ttff_p50_ms', label: 'p50' },
                { key: 'ttff_p90_ms', label: 'p90' },
              ]"
              :height="200"
              :y-format="msLabel"
              empty-text="No day with a measured time to first frame"
            />
            <growth-not-measured
              v-else
              reason="no frozen day in the range has its connect records measured"
            />
          </v-card-text>
        </v-card>

        <v-card class="mb-3">
          <v-card-title class="text-subtitle-1">By agent version</v-card-title>
          <v-card-text>
            <v-table density="compact" class="growth-wide">
              <thead>
                <tr>
                  <th class="text-left">Agent version</th>
                  <th class="text-right">Requested</th>
                  <th class="text-right">Reached Active</th>
                  <th class="text-right">Never Active</th>
                  <th class="text-left">Duration (Active, ended)</th>
                  <th class="text-left">Time to first frame</th>
                  <th class="text-right">Crashes</th>
                  <th class="text-right">Device-days</th>
                  <th class="text-right">Per 100 device-days</th>
                  <th class="text-right">Days</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="v in versionRows" :key="v.agent_version" :class="{ 'font-weight-medium': v.agent_version === '*' }">
                  <td>{{ v.agent_version === '*' ? 'every version' : v.agent_version }}</td>
                  <td class="text-right">{{ v.sessions?.requested ?? '—' }}</td>
                  <td class="text-right">
                    {{ v.sessions?.reached_active ?? '—' }}
                    <span v-if="shareOf(v) !== null" class="text-caption text-medium-emphasis">
                      ({{ shareOf(v) }})
                    </span>
                  </td>
                  <td class="text-right">{{ v.sessions?.not_reached ?? '—' }}</td>
                  <td class="text-caption">
                    <template v-if="v.sessions">{{ pooledLabel(v.sessions.duration_ms) }}</template>
                    <growth-not-measured v-else reason="remote module not mounted" />
                  </td>
                  <td class="text-caption">
                    <template v-if="v.ttff">{{ ttffLabel(v.ttff) }}</template>
                    <growth-not-measured v-else reason="no day with complete logs" />
                  </td>
                  <td class="text-right">{{ v.crashes ?? '—' }}</td>
                  <td class="text-right">{{ v.device_days ?? '—' }}</td>
                  <td class="text-right">
                    <template v-if="v.crashes_per_100_device_days != null">{{ v.crashes_per_100_device_days.toFixed(2) }}</template>
                    <span v-else class="text-caption text-medium-emphasis">n &lt; {{ report.n_floor }}</span>
                  </td>
                  <td class="text-right">{{ v.days }}</td>
                </tr>
              </tbody>
            </v-table>
          </v-card-text>
        </v-card>

        <!-- FR-102 P9c — the Active sessions by the carrier they rode. -->
        <v-card v-if="carriers.length" class="mb-3">
          <v-card-title class="text-subtitle-1 d-flex align-center flex-wrap" style="gap: 8px">
            By carrier
            <span class="text-caption text-medium-emphasis">
              how the session reached the device — recorded, never chosen; absent is unknown, never direct
            </span>
          </v-card-title>
          <v-card-text>
            <v-table density="compact" class="growth-wide">
              <thead>
                <tr>
                  <th class="text-left">Carrier</th>
                  <th class="text-right">Reached Active</th>
                  <th class="text-left">Duration</th>
                  <th class="text-left">Peak frame rate</th>
                  <th class="text-left">Average RTT</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="c in carriers" :key="c.carrier">
                  <td>{{ carrierLabel(c.carrier) }}</td>
                  <td class="text-right">{{ c.reached_active }}</td>
                  <td class="text-caption">{{ pooledLabel(c.duration_ms) }}</td>
                  <td class="text-caption">{{ pooledLabel(c.peak_fps, fps) }}</td>
                  <td class="text-caption">{{ pooledLabel(c.avg_rtt_ms) }}</td>
                </tr>
              </tbody>
            </v-table>
          </v-card-text>
        </v-card>

        <v-row dense class="mb-3">
          <v-col cols="12" md="5">
            <v-card class="fill-height">
              <v-card-title class="text-subtitle-1">How the sessions ended</v-card-title>
              <v-card-text>
                <v-table v-if="endReasons.length" density="compact">
                  <thead>
                    <tr>
                      <th class="text-left">Reached Active</th>
                      <th class="text-left">End reason</th>
                      <th class="text-right">Sessions</th>
                    </tr>
                  </thead>
                  <tbody>
                    <tr v-for="(c, i) in endReasons" :key="i">
                      <td>{{ c.reached_active ? 'yes' : 'no' }}</td>
                      <td>{{ c.reason }}</td>
                      <td class="text-right">{{ c.count }}</td>
                    </tr>
                  </tbody>
                </v-table>
                <span v-else class="text-caption text-medium-emphasis">No session in the range.</span>
              </v-card-text>
            </v-card>
          </v-col>
          <v-col cols="12" md="7">
            <v-card class="fill-height">
              <v-card-title class="text-subtitle-1">Distributions, every version</v-card-title>
              <v-card-text>
                <template v-if="report.totals.sessions">
                  <div class="text-body-2"><strong>Duration</strong> (Active, ended): {{ pooledLabel(report.totals.sessions.duration_ms) }}</div>
                  <div class="text-body-2"><strong>Peak frame rate</strong>: {{ pooledLabel(report.totals.sessions.peak_fps, fps) }}</div>
                  <div class="text-body-2"><strong>Average RTT</strong>: {{ pooledLabel(report.totals.sessions.avg_rtt_ms) }}</div>
                </template>
                <growth-not-measured v-else reason="remote module not mounted" />
                <div v-if="report.totals.ttff" class="text-body-2 mt-2">
                  <strong>Time to first frame</strong>: {{ ttffLabel(report.totals.ttff) }} ·
                  {{ report.totals.ttff.days_measured }} days measured · set aside: hidden tab
                  {{ report.totals.ttff.hidden }}, later attempts {{ report.totals.ttff.later_attempts }} ·
                  outcomes {{ outcomesLabel(report.totals.ttff.outcomes) }}
                </div>
                <div class="text-caption text-medium-emphasis mt-2">
                  A percentile marked ≈ is pooled across days from the daily histograms; a single day's is exact.
                  A device's version is read from its own logs of the day, else from its device row.
                </div>
              </v-card-text>
            </v-card>
          </v-col>
        </v-row>

        <v-card>
          <v-card-title class="text-subtitle-1">Day by day, every version</v-card-title>
          <v-card-text>
            <v-table density="compact" class="growth-wide">
              <thead>
                <tr>
                  <th class="text-left">Day</th>
                  <th class="text-right">Requested</th>
                  <th class="text-right">Reached Active</th>
                  <th class="text-right">Never Active</th>
                  <th class="text-right">Pending</th>
                  <th class="text-left">Duration p50</th>
                  <th class="text-left">TTFF p50 · p90 (n)</th>
                  <th class="text-right">Crashes</th>
                  <th class="text-right">Device-days</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="d in [...report.series].reverse()" :key="d.day">
                  <td>{{ d.day }}</td>
                  <td class="text-right">{{ d.requested ?? '—' }}</td>
                  <td class="text-right">{{ d.reached_active ?? '—' }}</td>
                  <td class="text-right">{{ d.not_reached ?? '—' }}</td>
                  <td class="text-right">{{ d.pending ?? '—' }}</td>
                  <td class="text-caption">{{ msLabel(d.duration_p50_ms) }}</td>
                  <td class="text-caption">
                    <template v-if="d.ttff_n == null"><growth-not-measured reason="logs expired before the freeze" /></template>
                    <template v-else-if="d.ttff_n === 0">no sample</template>
                    <template v-else>{{ msLabel(d.ttff_p50_ms) }} · {{ msLabel(d.ttff_p90_ms) }} ({{ d.ttff_n }})</template>
                  </td>
                  <td class="text-right">{{ d.crashes ?? '—' }}</td>
                  <td class="text-right">{{ d.device_days ?? '—' }}</td>
                </tr>
              </tbody>
            </v-table>
          </v-card-text>
        </v-card>
      </template>
    </template>
    <v-progress-circular v-else-if="loading" indeterminate size="24" />
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import {
  useGrowthStore,
  msLabel,
  pooledLabel,
  type QualityReport,
  type TtffWindow,
  type VersionQuality,
} from '@/stores/growth'
import GrowthNotMeasured from './GrowthNotMeasured.vue'
import TimeSeriesChart from '@/components/stats/TimeSeriesChart.vue'

const store = useGrowthStore()

const yesterday = (() => {
  const d = new Date()
  d.setUTCDate(d.getUTCDate() - 1)
  return d.toISOString().slice(0, 10)
})()
const since = ref(
  (() => {
    const d = new Date()
    d.setUTCDate(d.getUTCDate() - 28)
    return d.toISOString().slice(0, 10)
  })(),
)
const until = ref(yesterday)
const includeInternal = ref(false)
const loading = ref(false)
const message = ref<string | null>(null)
const report = ref<QualityReport | null>(null)

function fps(v: number): string {
  return `${v} fps`
}

function pct(share: number): string {
  return `${Math.round(share * 1000) / 10}%`
}

/** The reached-Active share of a version, only where the server computed one (n ≥ 30). */
function shareOf(v: VersionQuality): string | null {
  const s = v.sessions?.reached_share
  return typeof s === 'number' ? pct(s) : null
}

const endReasons = computed(() => report.value?.totals?.sessions?.by_end_reason ?? [])
const carriers = computed(() => report.value?.totals?.sessions?.by_carrier ?? [])

/** A carrier code as a reader reads it; an unknown code is shown verbatim. */
function carrierLabel(c: string): string {
  const map: Record<string, string> = {
    direct: 'Direct',
    turn: 'TURN relay',
    turn_local: 'TURN (device-local)',
    overlay_direct: 'Overlay — direct',
    overlay_turn: 'Overlay — TURN',
    derp: 'Overlay — DERP',
    org_relay: 'Overlay — org relay',
    unknown: 'Unknown',
  }
  return map[c] ?? c
}

function ttffLabel(t: TtffWindow): string {
  if (t.n === 0) return 'no sample'
  return `${t.exact ? '' : '≈ '}p50 ${msLabel(t.p50_ms)} · p90 ${msLabel(t.p90_ms)} (n ${t.n})`
}

function outcomesLabel(o: Record<string, number>): string {
  const parts = Object.entries(o).map(([k, v]) => `${k} ${v}`)
  return parts.length ? parts.join(' · ') : 'none'
}

/** The every-version row first, then each release as the server ordered them. */
const versionRows = computed<VersionQuality[]>(() => {
  const r = report.value
  if (!r) return []
  return r.totals ? [r.totals, ...r.by_agent_version] : r.by_agent_version
})

const ttffPoints = computed(() =>
  (report.value?.series ?? [])
    .filter((d) => typeof d.ttff_n === 'number' && d.ttff_n > 0)
    .map((d) => ({
      t: Math.floor(new Date(`${d.day}T00:00:00Z`).getTime() / 1000),
      ttff_p50_ms: d.ttff_p50_ms ?? null,
      ttff_p90_ms: d.ttff_p90_ms ?? null,
    })),
)

const cards = computed(() => {
  const t = report.value?.totals
  if (!t) return []
  const s = t.sessions
  const out: { label: string; value: string; note?: string }[] = [
    { label: 'Sessions requested', value: s ? String(s.requested) : '—' },
    {
      label: 'Reached Active',
      value: s ? String(s.reached_active) : '—',
      note: s ? (s.reached_share != null ? pct(s.reached_share) : `n < ${report.value?.n_floor ?? 30}`) : undefined,
    },
    { label: 'Never reached Active', value: s ? String(s.not_reached) : '—', note: s && s.pending ? `${s.pending} pending` : undefined },
    {
      label: 'Time to first frame',
      value: t.ttff && t.ttff.n > 0 ? `${t.ttff.exact ? '' : '≈ '}${msLabel(t.ttff.p50_ms)}` : '—',
      note: t.ttff ? `p90 ${msLabel(t.ttff.p90_ms)} · n ${t.ttff.n}` : 'not measured',
    },
    { label: 'Agent crashes', value: t.crashes != null ? String(t.crashes) : '—', note: t.device_days != null ? `${t.device_days} device-days` : undefined },
    {
      label: 'Per 100 device-days',
      value: t.crashes_per_100_device_days != null ? t.crashes_per_100_device_days.toFixed(2) : '—',
      note: t.crashes_per_100_device_days == null ? `n < ${report.value?.n_floor ?? 30} device-days` : undefined,
    },
  ]
  return out
})

async function load() {
  loading.value = true
  message.value = null
  try {
    report.value = await store.fetchQuality({
      since: since.value,
      until: until.value,
      includeInternal: includeInternal.value,
    })
  } catch (e) {
    message.value = e instanceof Error ? e.message : 'quality failed'
  } finally {
    loading.value = false
  }
}

onMounted(load)
</script>

<style scoped>
.growth-wide {
  max-width: 100%;
  overflow-x: auto;
}
</style>
