<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- FR-102 P5 — the newsletter: the list over time and the per-issue
       table. Delivery comes from SendGrid's signed Event Webhook (delivered,
       bounces, drops, spam reports — never opens or clicks); visits from
       purestat by the issue's campaign key; sign-ups by that key. Three
       cells, never flattened: a value, a "not measured" chip with its
       reason, and a measured zero. Subjects and sources are text, never
       v-html. -->
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
      <growth-not-measured v-if="report.not_measured" :reason="report.not_measured.not_measured" />
      <template v-else>
        <div class="text-caption text-medium-emphasis mb-2">
          {{ report.since }} → {{ report.until }} · sign-ups exclude {{ report.excluded.users }} users,
          {{ report.excluded.tenants }} orgs
          <template v-if="report.include_internal"> · INTERNAL INCLUDED</template>
          · rates from n ≥ {{ report.n_floor }}
        </div>

        <div class="d-flex flex-wrap mb-3" style="gap: 6px">
          <v-chip size="small" variant="tonal" :color="report.webhook_configured ? 'success' : 'grey'">
            {{
              report.webhook_configured
                ? 'delivery: SendGrid Event Webhook verified'
                : 'delivery not measured — no usable webhook key on this server'
            }}
          </v-chip>
          <v-chip size="small" variant="tonal" :color="report.link_tags ? 'success' : 'grey'">
            {{ report.link_tags ? 'links tagged per issue' : 'link tags off' }}
          </v-chip>
          <growth-not-measured
            v-if="report.web_not_measured"
            :reason="`visits: ${report.web_not_measured.not_measured}`"
          />
          <v-chip v-if="report.web_truncated" size="small" color="warning" variant="tonal">
            purestat breakdown cut at its page bound
          </v-chip>
          <v-chip v-if="report.issues_truncated" size="small" color="warning" variant="tonal">
            newest {{ report.issues.length }} issues shown
          </v-chip>
        </div>

        <template v-if="report.list">
          <v-row dense class="mb-2">
            <v-col v-for="c in sizeCards" :key="c.label" cols="6" sm="3">
              <v-card variant="tonal">
                <v-card-text>
                  <div class="text-caption text-medium-emphasis">{{ c.label }}</div>
                  <div class="text-h6">{{ c.value }}</div>
                </v-card-text>
              </v-card>
            </v-col>
          </v-row>

          <v-card class="mb-3">
            <v-card-title class="text-subtitle-1 d-flex align-center flex-wrap" style="gap: 8px">
              The list over the range
              <span class="text-caption text-medium-emphasis">
                {{ report.list.totals.new }} new · {{ report.list.totals.confirmed }} confirmed ·
                {{ report.list.totals.unsubscribed }} unsubscribed
                ({{ report.list.totals.unsubscribed_via_issue }} through an issue's link) ·
                {{ report.list.totals.suppressed }} suppressed
              </span>
            </v-card-title>
            <v-card-text>
              <time-series-chart
                :points="listPoints"
                :series="listSeries"
                :height="180"
                empty-text="No changes in the range"
              />
              <v-table v-if="report.list.by_source.length" density="compact" class="growth-wide mt-2">
                <thead>
                  <tr>
                    <th class="text-left">Source</th>
                    <th class="text-right">New</th>
                    <th class="text-right">Confirmed</th>
                    <th class="text-right">Unsubscribed</th>
                  </tr>
                </thead>
                <tbody>
                  <tr v-for="s in report.list.by_source" :key="s.source">
                    <td>{{ s.source }}</td>
                    <td class="text-right">{{ s.new }}</td>
                    <td class="text-right">{{ s.confirmed }}</td>
                    <td class="text-right">{{ s.unsubscribed }}</td>
                  </tr>
                </tbody>
              </v-table>
            </v-card-text>
          </v-card>
        </template>

        <v-table v-if="report.issues.length" density="compact" class="growth-wide">
          <thead>
            <tr>
              <th class="text-left">Issue</th>
              <th class="text-right" title="the send ledger's rows">Recipients</th>
              <th class="text-right" title="accepted by SendGrid — not delivered">Accepted</th>
              <th class="text-right">Delivered</th>
              <th class="text-right" title="all bounces (hard)">Bounced</th>
              <th class="text-right" title="spam reports">Complaints</th>
              <th class="text-right" title="through this issue's own unsubscribe link">Unsubscribed</th>
              <th class="text-right" title="purestat, by the issue's campaign key">Visitors</th>
              <th class="text-right" title="sign-ups carrying the issue's campaign key">Sign-ups</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="i in report.issues" :key="i.slug">
              <td>
                <div>
                  {{ i.subject }}
                  <v-chip
                    v-if="i.test_audience != null"
                    size="x-small"
                    variant="tonal"
                    color="info"
                    class="ml-1"
                    title="sent only to the confirmed subscribers among its audience"
                  >test · {{ i.test_audience }}</v-chip>
                </div>
                <div class="text-caption text-medium-emphasis">
                  {{ i.slug }} · {{ i.status }}<template v-if="i.sent_at"> · {{ i.sent_at.slice(0, 10) }}</template>
                  <template v-if="i.campaign"> · {{ i.campaign }}</template>
                </div>
              </td>
              <td class="text-right">
                {{ i.ledger.recipients }}
                <div v-if="i.ledger.suppressed || i.ledger.failed" class="text-caption text-medium-emphasis">
                  <template v-if="i.ledger.suppressed">{{ i.ledger.suppressed }} suppressed</template>
                  <template v-if="i.ledger.suppressed && i.ledger.failed"> · </template>
                  <template v-if="i.ledger.failed">{{ i.ledger.failed }} failed</template>
                </div>
              </td>
              <td class="text-right">{{ i.ledger.accepted }}</td>
              <template v-if="isNotMeasured(i.delivery)">
                <td colspan="3" class="text-right">
                  <v-chip size="x-small" variant="tonal" color="grey" :title="i.delivery.not_measured">
                    not measured
                  </v-chip>
                </td>
              </template>
              <template v-else>
                <td class="text-right">
                  {{ i.delivery.delivered }}
                  <div v-if="rateLabel(i.rates.delivered)" class="text-caption text-medium-emphasis">
                    {{ rateLabel(i.rates.delivered) }}
                  </div>
                </td>
                <td class="text-right">
                  {{ i.delivery.bounced }} ({{ i.delivery.hard_bounced }})
                  <div v-if="rateLabel(i.rates.hard_bounce)" class="text-caption text-medium-emphasis">
                    {{ rateLabel(i.rates.hard_bounce) }} hard
                  </div>
                </td>
                <td class="text-right">{{ i.delivery.complaints }}</td>
              </template>
              <td class="text-right">
                {{ i.unsubscribed_via }}
                <div v-if="rateLabel(i.rates.unsubscribe)" class="text-caption text-medium-emphasis">
                  {{ rateLabel(i.rates.unsubscribe) }}
                </div>
              </td>
              <td class="text-right">
                <v-chip
                  v-if="isNotMeasured(i.web)"
                  size="x-small"
                  variant="tonal"
                  color="grey"
                  :title="i.web.not_measured"
                >not measured</v-chip>
                <span v-else>{{ i.web.visitors }}</span>
              </td>
              <td class="text-right">
                {{ i.signups }}
                <div v-if="rateLabel(i.rates.signup_per_visitor)" class="text-caption text-medium-emphasis">
                  {{ rateLabel(i.rates.signup_per_visitor) }} of visitors
                </div>
              </td>
            </tr>
          </tbody>
        </v-table>
        <v-alert v-else type="info" variant="tonal" density="compact">
          No issue was sent in this range.
        </v-alert>
      </template>
    </template>
    <div v-else-if="loading" class="pa-4"><v-progress-circular indeterminate size="24" /></div>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import TimeSeriesChart from '@/components/stats/TimeSeriesChart.vue'
import { useGrowthStore, isNotMeasured, rateLabel, type NewsletterReport } from '@/stores/growth'
import GrowthNotMeasured from './GrowthNotMeasured.vue'

const store = useGrowthStore()

function utcDay(offsetDays: number): string {
  const d = new Date()
  d.setUTCDate(d.getUTCDate() + offsetDays)
  return d.toISOString().slice(0, 10)
}

const today = utcDay(0)
const until = ref(utcDay(-1))
const since = ref(utcDay(-90))
const includeInternal = ref(false)
const loading = ref(false)
const message = ref<string | null>(null)
const report = ref<NewsletterReport | null>(null)

const sizeCards = computed(() => {
  const s = report.value?.list?.size
  if (!s) return []
  return [
    { label: 'Mailable now', value: s.mailable },
    { label: 'Awaiting confirmation', value: s.pending },
    { label: 'Unsubscribed', value: s.unsubscribed },
    { label: 'Suppressed (bounce · spam report)', value: s.suppressed },
  ]
})

const listSeries = [
  { key: 'new', label: 'New' },
  { key: 'confirmed', label: 'Confirmed' },
  { key: 'unsubscribed', label: 'Unsubscribed' },
  { key: 'suppressed', label: 'Suppressed' },
]

const listPoints = computed(() =>
  (report.value?.list?.days ?? []).map((d) => ({
    t: Math.floor(new Date(`${d.day}T00:00:00Z`).getTime() / 1000),
    new: d.new,
    confirmed: d.confirmed,
    unsubscribed: d.unsubscribed,
    suppressed: d.suppressed,
  })),
)

async function load() {
  loading.value = true
  message.value = null
  try {
    report.value = await store.fetchNewsletter({
      since: since.value || undefined,
      until: until.value || undefined,
      includeInternal: includeInternal.value,
    })
  } catch (e) {
    message.value = e instanceof Error ? e.message : 'newsletter failed'
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
