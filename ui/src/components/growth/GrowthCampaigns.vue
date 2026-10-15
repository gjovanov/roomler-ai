<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- FR-102 P6 — the Campaigns table: per normalised campaign key,
       purestat's web plane (live, for the exact range) beside the account
       funnel. Three cells, never flattened: a value, a "not measured" chip
       with its reason, and a measured zero. Every key field is client-chosen
       text and is rendered with {{ }}, never v-html. -->
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
        :max="today"
      />
      <v-select
        v-model="by"
        :items="depthItems"
        label="Key depth"
        density="compact"
        hide-details
        style="max-width: 180px"
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
        {{ report.since }} → {{ report.until }} · by {{ report.by }} · excluded
        {{ report.excluded.users }} users, {{ report.excluded.tenants }} orgs
        <template v-if="report.include_internal"> · INTERNAL INCLUDED</template>
        · rates from n ≥ {{ report.n_floor }}
      </div>

      <div class="d-flex flex-wrap mb-3" style="gap: 6px">
        <growth-not-measured
          v-if="report.web_not_measured"
          :reason="`web plane: ${report.web_not_measured.not_measured}`"
        />
        <growth-not-measured
          v-if="report.conversions"
          :reason="`conversions: ${report.conversions.not_measured}`"
        />
        <v-chip size="small" variant="tonal" :color="pullColor">
          {{ pullLabel }}
        </v-chip>
        <v-chip v-if="report.web_truncated" size="small" color="warning" variant="tonal">
          purestat breakdown cut at its page bound — busiest keys only
        </v-chip>
        <v-chip v-if="report.account_truncated" size="small" color="warning" variant="tonal">
          more sign-ups than one read loads — oldest only
        </v-chip>
        <v-chip v-if="report.rows_truncated" size="small" color="warning" variant="tonal">
          busiest {{ report.rows.length }} keys shown
        </v-chip>
      </div>

      <v-row dense class="mb-2">
        <v-col cols="12" sm="4">
          <v-card variant="tonal">
            <v-card-text>
              <div class="text-caption text-medium-emphasis">Visitors (all, unique over the range)</div>
              <div v-if="isNotMeasured(report.totals.web)">
                <growth-not-measured :reason="report.totals.web.not_measured" />
              </div>
              <div v-else class="text-h6">{{ report.totals.web.visitors }}</div>
            </v-card-text>
          </v-card>
        </v-col>
        <v-col cols="12" sm="4">
          <v-card variant="tonal">
            <v-card-text>
              <div class="text-caption text-medium-emphasis">Sign-ups (with a campaign key)</div>
              <div class="text-h6">
                {{ report.totals.signups }}
                <span class="text-body-2 text-medium-emphasis">({{ report.totals.keyed_signups }} keyed)</span>
              </div>
            </v-card-text>
          </v-card>
        </v-col>
        <v-col cols="12" sm="4">
          <v-card variant="tonal">
            <v-card-text>
              <div class="text-caption text-medium-emphasis">Coverage (purestat sign-up goals ÷ server sign-ups)</div>
              <div v-if="isNotMeasured(report.totals.coverage)">
                <growth-not-measured :reason="report.totals.coverage.not_measured" />
              </div>
              <div v-else class="text-h6">
                {{ coverageLabel(report.totals.coverage) }}
              </div>
            </v-card-text>
          </v-card>
        </v-col>
      </v-row>

      <v-table v-if="report.rows.length" density="compact" class="growth-wide">
        <thead>
          <tr>
            <th v-for="c in keyColumns" :key="c" class="text-left">{{ c }}</th>
            <th class="text-right">Visitors</th>
            <th class="text-right">Visits</th>
            <th class="text-right">Pageviews</th>
            <th class="text-right">Reads</th>
            <th class="text-right" title="signup · subscribe · install-copy · github-outbound">Goals</th>
            <th class="text-right">Sign-ups</th>
            <th class="text-right">Orgs</th>
            <th class="text-right">Activated</th>
            <th class="text-right">First value</th>
            <th class="text-right" title="the org's first invoice paid above 0 — no window: paid so far">Paid</th>
            <th class="text-left">Reach</th>
            <th class="text-right">Coverage</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(r, i) in report.rows" :key="i">
            <td v-for="c in keyColumns" :key="c">
              {{ keyField(r, c) ?? '—' }}
              <v-chip
                v-if="c === 'source' && r.group === 'other'"
                size="x-small"
                variant="tonal"
                class="ml-1"
              >other</v-chip>
            </td>
            <template v-if="isNotMeasured(r.web)">
              <td colspan="5">
                <v-chip size="x-small" variant="tonal" color="grey" :title="r.web.not_measured">
                  not measured
                </v-chip>
              </td>
            </template>
            <template v-else>
              <td class="text-right">
                {{ r.web.visitors }}
                <span
                  v-if="r.web.spellings > 1"
                  class="text-caption text-medium-emphasis"
                  :title="`${r.web.spellings} spellings folded into this key; the visitors are their sum`"
                >Σ{{ r.web.spellings }}</span>
                <div v-if="rateLabel(r.rates.signup_per_visitor)" class="text-caption text-medium-emphasis">
                  {{ rateLabel(r.rates.signup_per_visitor) }} sign up
                </div>
              </td>
              <td class="text-right">{{ r.web.visits }}</td>
              <td class="text-right">{{ r.web.pageviews }}</td>
              <td class="text-right">{{ r.web.reads }}</td>
              <td class="text-right">
                <v-chip
                  v-if="isNotMeasured(r.web.goals)"
                  size="x-small"
                  variant="tonal"
                  color="grey"
                  :title="r.web.goals.not_measured"
                >not measured</v-chip>
                <span v-else :title="'signup · subscribe · install-copy · github-outbound'">
                  {{ r.web.goals.signup }} · {{ r.web.goals.subscribe }} · {{ r.web.goals.install_copy }} ·
                  {{ r.web.goals.github_outbound }}
                </span>
              </td>
            </template>
            <td class="text-right">{{ r.signups }}</td>
            <td class="text-right">{{ r.org_created.count }}</td>
            <td class="text-right">
              <template v-if="r.activated">
                {{ r.activated.count }}
                <div class="text-caption text-medium-emphasis">
                  {{ stepNote(r.activated, r.rates.activation) }}
                </div>
              </template>
              <v-chip
                v-else
                size="x-small"
                variant="tonal"
                color="grey"
                :title="report.not_measured?.activated"
              >not measured</v-chip>
            </td>
            <td class="text-right">
              <template v-if="r.first_value">
                {{ r.first_value.count }}
                <div class="text-caption text-medium-emphasis">
                  {{ stepNote(r.first_value, r.rates.first_value) }}
                </div>
              </template>
              <v-chip
                v-else
                size="x-small"
                variant="tonal"
                color="grey"
                :title="report.not_measured?.first_value"
              >not measured</v-chip>
            </td>
            <td class="text-right">
              <template v-if="r.paid">
                {{ r.paid.count }}
                <div class="text-caption text-medium-emphasis">
                  {{ stepNote(r.paid, r.rates.paid) }}
                </div>
              </template>
              <v-chip
                v-else
                size="x-small"
                variant="tonal"
                color="grey"
                :title="report.not_measured?.paid"
              >not measured</v-chip>
            </td>
            <td>
              <v-chip
                v-if="isNotMeasured(r.reach)"
                size="x-small"
                variant="tonal"
                color="grey"
                :title="r.reach.not_measured"
              >not measured</v-chip>
              <template v-else>
                <div v-for="pr in r.reach" :key="pr.platform" class="text-caption">
                  {{ pr.platform }} · {{ pr.posts }} post{{ pr.posts === 1 ? '' : 's' }}
                  <template v-if="latestReach(pr.at)">
                    · {{ latestReach(pr.at)?.snapshot }}: {{ metricsLabel(latestReach(pr.at)?.metrics ?? {}) }}
                  </template>
                  <template v-else> · no snapshot yet</template>
                </div>
              </template>
            </td>
            <td class="text-right">
              <v-chip
                v-if="isNotMeasured(r.coverage)"
                size="x-small"
                variant="tonal"
                color="grey"
                :title="r.coverage.not_measured"
              >not measured</v-chip>
              <span v-else>{{ coverageLabel(r.coverage) }}</span>
            </td>
          </tr>
        </tbody>
      </v-table>
      <v-alert v-else type="info" variant="tonal" density="compact">
        No campaign key in this range — neither purestat nor a sign-up carried one.
      </v-alert>
    </template>
    <div v-else-if="loading" class="pa-4"><v-progress-circular indeterminate size="24" /></div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import {
  useGrowthStore,
  isNotMeasured,
  latestReach,
  metricsLabel,
  rateLabel,
  type CampaignRow,
  type CampaignsReport,
  type Coverage,
  type KeyDepth,
  type Rate,
  type StepCounts,
} from '@/stores/growth'
import GrowthNotMeasured from './GrowthNotMeasured.vue'

const store = useGrowthStore()

function utcDay(offsetDays: number): string {
  const d = new Date()
  d.setUTCDate(d.getUTCDate() + offsetDays)
  return d.toISOString().slice(0, 10)
}

const today = utcDay(0)
const until = ref(utcDay(-1))
const since = ref(utcDay(-28))
const by = ref<KeyDepth>('campaign')
const includeInternal = ref(false)
const loading = ref(false)
const message = ref<string | null>(null)
const report = ref<CampaignsReport | null>(null)

const depthItems = [
  { title: 'Source', value: 'source' },
  { title: 'Medium', value: 'medium' },
  { title: 'Campaign', value: 'campaign' },
  { title: 'Content', value: 'content' },
]

const DEPTH_FIELDS: Record<KeyDepth, (keyof CampaignRow['key'])[]> = {
  source: ['source'],
  medium: ['source', 'medium'],
  campaign: ['source', 'medium', 'campaign'],
  content: ['source', 'medium', 'campaign', 'content'],
}

const keyColumns = computed(() => DEPTH_FIELDS[report.value?.by ?? by.value])

function keyField(r: CampaignRow, field: keyof CampaignRow['key']): string | null {
  return r.key[field]
}

/** `3 settled · 1 pending`, or the rate with its n once n ≥ 30. */
function stepNote(s: StepCounts, rate: Rate | null): string {
  const pct = rateLabel(rate)
  const head = pct ? `${pct} of ${s.settled}` : `${s.settled} settled`
  return s.pending ? `${head} · ${s.pending} pending` : head
}

/** `1 / 1`, plus the ratio once the server gives one (n ≥ 30). */
function coverageLabel(c: Coverage): string {
  const counts = `${c.purestat_signup_goals} / ${c.server_signups}`
  return typeof c.ratio === 'number' ? `${Math.round(c.ratio * 1000) / 10}% (${counts})` : counts
}

const pullColor = computed(() => {
  const p = report.value?.pulls
  if (!p || p.not_measured) return 'grey'
  if (p.failed) return 'error'
  return p.not_pulled ? 'warning' : 'success'
})

const pullLabel = computed(() => {
  const p = report.value?.pulls
  if (!p) return ''
  if (p.not_measured) return `daily pull: ${p.not_measured}`
  let s = `daily pull: ${p.pulled}/${p.days} days pulled`
  if (p.failed) s += ` · ${p.failed} failed`
  if (p.not_pulled) s += ` · ${p.not_pulled} not pulled`
  if (p.last_error) s += ` · last error: ${p.last_error}`
  return s
})

async function load() {
  loading.value = true
  message.value = null
  try {
    report.value = await store.fetchCampaigns({
      since: since.value || undefined,
      until: until.value || undefined,
      by: by.value,
      includeInternal: includeInternal.value,
    })
  } catch (e) {
    message.value = e instanceof Error ? e.message : 'campaigns failed'
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
