<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- FR-102 P4 — billed revenue. Every billed figure is what Stripe billed
       (`billing_events`), beside the list-price figure, which stays and
       says it is an ESTIMATE. Three cells, never flattened: a value, a
       "not measured" chip with its reason, and a measured zero; a month
       before the first billing row is "not on record", never 0. Org names
       are client-chosen text and are rendered with {{ }}, never v-html. -->
  <div>
    <div class="d-flex align-center flex-wrap mb-3" style="gap: 12px">
      <v-text-field
        v-model="since"
        type="month"
        label="Since (month)"
        density="compact"
        hide-details
        style="max-width: 180px"
        :max="until"
      />
      <v-text-field
        v-model="until"
        type="month"
        label="Until (month)"
        density="compact"
        hide-details
        style="max-width: 180px"
        :max="thisMonth"
      />
      <v-switch
        v-model="includeInternal"
        label="Include internal (live)"
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
        {{ report.since }} → {{ report.until }} · as of {{ report.as_of.slice(0, 16).replace('T', ' ') }}Z ·
        excluded {{ report.excluded.users }} users, {{ report.excluded.tenants }} orgs
        <template v-if="report.include_internal"> · INTERNAL INCLUDED (every month live)</template>
      </div>

      <div class="d-flex flex-wrap mb-3" style="gap: 6px">
        <growth-not-measured v-if="isNotMeasured(report.billed)" :reason="report.billed.not_measured" />
        <template v-else>
          <v-chip size="small" variant="tonal">on record since {{ report.billed.record.since.slice(0, 10) }}</v-chip>
          <v-chip
            v-if="report.billed.record.backfilled_at"
            size="small"
            variant="tonal"
            color="success"
          >Stripe history read {{ report.billed.record.backfilled_at.slice(0, 10) }}</v-chip>
          <v-chip v-else size="small" variant="tonal" color="warning">
            no backfill yet — billed figures start at the webhook
          </v-chip>
          <v-chip v-if="report.billed.record.truncated" size="small" variant="tonal" color="warning">
            more rows than one read loads — oldest only
          </v-chip>
        </template>
      </div>

      <v-row dense class="mb-2">
        <v-col cols="12" sm="6" md="3">
          <v-card variant="tonal" class="fill-height">
            <v-card-text>
              <div class="text-caption text-medium-emphasis">Billed MRR now</div>
              <growth-not-measured v-if="isNotMeasured(report.billed)" :reason="report.billed.not_measured" />
              <template v-else>
                <div v-for="c in report.billed.now" :key="c.currency" class="text-h6">
                  {{ money(c.mrr_cents) }} {{ c.currency.toUpperCase() }}
                  <span class="text-body-2 text-medium-emphasis">
                    · {{ c.paying_orgs }} paying org{{ c.paying_orgs === 1 ? '' : 's' }}
                  </span>
                </div>
                <div v-if="!report.billed.now.length" class="text-h6">0 <span class="text-body-2 text-medium-emphasis">· no paying org</span></div>
              </template>
            </v-card-text>
          </v-card>
        </v-col>
        <v-col cols="12" sm="6" md="3">
          <v-card variant="tonal" class="fill-height">
            <v-card-text>
              <div class="text-caption text-medium-emphasis">MRR — list-price ESTIMATE</div>
              <div class="text-h6">{{ money(report.estimate.cents) }}</div>
              <div class="text-caption text-medium-emphasis">{{ report.estimate.basis }}</div>
            </v-card-text>
          </v-card>
        </v-col>
        <v-col cols="12" sm="6" md="3">
          <v-card variant="tonal" class="fill-height">
            <v-card-text>
              <div class="text-caption text-medium-emphasis">Trial → paid (trials started in the range)</div>
              <template v-if="!isNotMeasured(report.billed)">
                <div class="text-h6">
                  {{ report.billed.trial_to_paid.converted }} / {{ report.billed.trial_to_paid.trials }}
                </div>
                <div class="text-caption text-medium-emphasis">
                  {{ rateLabel(report.billed.trial_to_paid.rate) ?? `n = ${report.billed.trial_to_paid.rate.n} < 30, no rate` }}
                </div>
              </template>
              <growth-not-measured v-else :reason="report.billed.not_measured" />
            </v-card-text>
          </v-card>
        </v-col>
        <v-col cols="12" sm="6" md="3">
          <v-card variant="tonal" class="fill-height">
            <v-card-text>
              <div class="text-caption text-medium-emphasis">In the range</div>
              <template v-if="!isNotMeasured(report.billed)">
                <div class="text-body-2">{{ report.billed.totals.invoices_paid }} invoices paid</div>
                <div v-for="b in report.billed.totals.billed" :key="b.currency" class="text-body-2">
                  billed {{ money(b.cents) }} {{ b.currency.toUpperCase() }}
                </div>
                <div class="text-body-2">
                  {{ report.billed.totals.new_paying_orgs }} first payments ·
                  {{ report.billed.totals.payment_failures }} failed payments ·
                  {{ report.billed.totals.cancellations }} cancellations
                </div>
              </template>
              <growth-not-measured v-else :reason="report.billed.not_measured" />
            </v-card-text>
          </v-card>
        </v-col>
      </v-row>

      <!-- ── Month by month ─────────────────────────────────────────── -->
      <v-card v-if="!isNotMeasured(report.billed)" class="mb-3">
        <v-card-title class="text-subtitle-1">
          Billed MRR, month by month
          <span class="text-caption text-medium-emphasis ml-2">
            MRR at each month's end; a seat change moves it when an invoice bills it
          </span>
        </v-card-title>
        <v-card-text>
          <v-table density="compact" class="growth-wide">
            <thead>
              <tr>
                <th class="text-left">Month</th>
                <th class="text-left">Currency</th>
                <th class="text-right">MRR</th>
                <th class="text-right">Paying</th>
                <th class="text-right">New</th>
                <th class="text-right">Expansion</th>
                <th class="text-right">Contraction</th>
                <th class="text-right">Churned</th>
                <th class="text-right">Net new</th>
                <th class="text-right">Invoices</th>
                <th class="text-right">Billed</th>
              </tr>
            </thead>
            <tbody>
              <template v-for="m in report.billed.months" :key="m.month">
                <tr v-if="!m.on_record">
                  <td>{{ m.month }}</td>
                  <td colspan="10">
                    <v-chip size="x-small" variant="tonal" color="grey">
                      not measured — before the first billing event on record
                    </v-chip>
                  </td>
                </tr>
                <tr v-else-if="!m.currencies.length">
                  <td>
                    {{ m.month }}
                    <v-chip v-if="m.frozen" size="x-small" variant="tonal" class="ml-1">frozen</v-chip>
                    <v-chip v-if="m.partial" size="x-small" variant="tonal" color="info" class="ml-1">open</v-chip>
                  </td>
                  <td colspan="10" class="text-medium-emphasis">nothing billed, nothing moved</td>
                </tr>
                <template v-else>
                  <tr v-for="c in m.currencies" :key="`${m.month}-${c.currency}`">
                    <td>
                      {{ m.month }}
                      <v-chip v-if="m.frozen" size="x-small" variant="tonal" class="ml-1">frozen</v-chip>
                      <v-chip v-if="m.partial" size="x-small" variant="tonal" color="info" class="ml-1">open</v-chip>
                    </td>
                    <td>{{ c.currency.toUpperCase() }}</td>
                    <td class="text-right">{{ money(c.mrr_cents) }}</td>
                    <td class="text-right">{{ c.paying_orgs }}</td>
                    <td class="text-right">{{ money(c.new_cents) }} <span class="text-caption">({{ c.new_orgs }})</span></td>
                    <td class="text-right">{{ money(c.expansion_cents) }}</td>
                    <td class="text-right">{{ money(c.contraction_cents) }}</td>
                    <td class="text-right">{{ money(c.churned_cents) }} <span class="text-caption">({{ c.churned_orgs }})</span></td>
                    <td class="text-right">{{ money(c.net_new_cents) }}</td>
                    <td class="text-right">{{ c.invoices_paid }}</td>
                    <td class="text-right">{{ money(c.billed_cents) }}</td>
                  </tr>
                </template>
              </template>
            </tbody>
          </v-table>
        </v-card-text>
      </v-card>

      <!-- ── By the owner's channel ─────────────────────────────────── -->
      <v-card v-if="!isNotMeasured(report.billed) && report.billed.by_channel.length" class="mb-3">
        <v-card-title class="text-subtitle-1">By the owner's acquisition channel</v-card-title>
        <v-card-text>
          <v-table density="compact" class="growth-wide">
            <thead>
              <tr>
                <th class="text-left">Channel</th>
                <th class="text-left">Currency</th>
                <th class="text-right">Paying now</th>
                <th class="text-right">MRR now</th>
                <th class="text-right">First payments</th>
                <th class="text-right">Churned</th>
                <th class="text-right">Trials → paid</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="r in report.billed.by_channel" :key="`${r.channel}-${r.currency}`">
                <td>{{ r.channel }}</td>
                <td>{{ r.currency.toUpperCase() }}</td>
                <td class="text-right">{{ r.paying_orgs }}</td>
                <td class="text-right">{{ money(r.mrr_cents) }}</td>
                <td class="text-right">{{ r.new_paying_orgs }}</td>
                <td class="text-right">{{ r.churned_orgs }}</td>
                <td class="text-right">{{ r.converted }} / {{ r.trials }}</td>
              </tr>
            </tbody>
          </v-table>
        </v-card-text>
      </v-card>

      <!-- ── Upgrade intent ─────────────────────────────────────────── -->
      <v-card class="mb-3">
        <v-card-title class="text-subtitle-1">
          Upgrade intent — plan-limit collisions
          <span class="text-caption text-medium-emphasis ml-2">
            {{ report.upgrade_intent.collisions }} in {{ report.upgrade_intent.orgs }} org{{ report.upgrade_intent.orgs === 1 ? '' : 's' }}
          </span>
        </v-card-title>
        <v-card-text>
          <div v-if="report.upgrade_intent.by_limit.length" class="d-flex flex-wrap mb-2" style="gap: 6px">
            <v-chip v-for="l in report.upgrade_intent.by_limit" :key="l.limit" size="small" variant="tonal">
              {{ l.limit }} · {{ l.count }}
            </v-chip>
          </div>
          <v-table v-if="report.upgrade_intent.top.length" density="compact" class="growth-wide">
            <thead>
              <tr>
                <th class="text-left">Org</th>
                <th class="text-left">Plan</th>
                <th class="text-left">Limits</th>
                <th class="text-right">Collisions</th>
                <th class="text-right">Days</th>
                <th class="text-left">Last</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="c in report.upgrade_intent.top" :key="c.tenant_id">
                <td>{{ c.name || c.tenant_id }}</td>
                <td>{{ c.plan }}</td>
                <td>{{ c.limits.map((l) => `${l.limit} ${l.count}`).join(' · ') }}</td>
                <td class="text-right">{{ c.total }}</td>
                <td class="text-right">{{ c.days }}</td>
                <td>{{ c.last_day }}</td>
              </tr>
            </tbody>
          </v-table>
          <span v-else class="text-medium-emphasis text-body-2">No org hit a plan limit in the range.</span>
        </v-card-text>
      </v-card>

      <!-- ── Per org ────────────────────────────────────────────────── -->
      <v-card>
        <v-card-title class="text-subtitle-1">
          Per org
          <span class="text-caption text-medium-emphasis ml-2">
            billed MRR beside the list-price ESTIMATE; margin = billed MRR − 30-day cost to serve
          </span>
        </v-card-title>
        <v-card-text>
          <div class="text-caption text-medium-emphasis mb-2">
            <template v-if="isNotMeasured(report.cost)">Cost to serve {{ notMeasuredLabel(report.cost) }}.</template>
            <template v-else-if="!report.cost.priced">
              Cost to serve: no metered resource carries a price, so every cost reads "not priced".
            </template>
            <template v-else>Cost to serve over {{ report.cost.range }} in {{ report.cost.currency ?? '—' }}.</template>
          </div>
          <v-table v-if="report.orgs.length" density="compact" class="growth-wide">
            <thead>
              <tr>
                <th class="text-left">Org</th>
                <th class="text-left">Plan</th>
                <th class="text-left">Channel</th>
                <th class="text-right">Seats</th>
                <th class="text-left">Paid since</th>
                <th class="text-right">Billed MRR</th>
                <th class="text-right">List price (est.)</th>
                <th class="text-right">Cost 30d</th>
                <th class="text-right">Margin</th>
                <th class="text-right">Collisions</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="o in report.orgs" :key="o.tenant_id">
                <td>
                  {{ o.name || o.tenant_id }}
                  <v-chip v-if="o.deleted" size="x-small" variant="tonal" class="ml-1">deleted</v-chip>
                </td>
                <td>{{ o.plan }}<span v-if="o.billing_status" class="text-caption text-medium-emphasis"> · {{ o.billing_status }}</span></td>
                <td>{{ o.channel ?? 'unknown' }}</td>
                <td class="text-right">{{ o.seats }}</td>
                <td>{{ o.paid_since ? o.paid_since.slice(0, 10) : '—' }}</td>
                <td class="text-right">
                  <template v-if="o.billed_mrr_cents != null">
                    {{ money(o.billed_mrr_cents) }} {{ (o.currency ?? '').toUpperCase() }}
                  </template>
                  <span v-else class="text-medium-emphasis" title="no paid invoice on record">no invoice</span>
                </td>
                <td class="text-right text-medium-emphasis">{{ money(o.list_price_mrr_cents) }}</td>
                <td class="text-right">
                  <template v-if="o.cost_30d != null">{{ o.cost_30d.toFixed(2) }}</template>
                  <span v-else class="text-medium-emphasis">not priced</span>
                </td>
                <td class="text-right">
                  <template v-if="o.margin_cents != null">{{ money(o.margin_cents) }}</template>
                  <span v-else class="text-medium-emphasis">—</span>
                </td>
                <td class="text-right">{{ o.collisions }}</td>
              </tr>
            </tbody>
          </v-table>
          <span v-else class="text-medium-emphasis text-body-2">
            No org has a billing history, a paid plan or a plan-limit collision.
          </span>
          <div v-if="report.orgs_truncated" class="text-caption text-medium-emphasis mt-1">
            Showing the first {{ report.orgs.length }} orgs.
          </div>
        </v-card-text>
      </v-card>
    </template>
    <div v-else-if="loading" class="pa-4"><v-progress-circular indeterminate size="24" /></div>
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import {
  useGrowthStore,
  isNotMeasured,
  notMeasuredLabel,
  rateLabel,
  type RevenueReport,
} from '@/stores/growth'
import GrowthNotMeasured from './GrowthNotMeasured.vue'

const store = useGrowthStore()

/** `YYYY-MM` of the UTC month `back` months ago. */
function utcMonth(back: number): string {
  const d = new Date()
  d.setUTCDate(1)
  d.setUTCMonth(d.getUTCMonth() - back)
  return d.toISOString().slice(0, 7)
}

const thisMonth = utcMonth(0)
const until = ref(thisMonth)
const since = ref(utcMonth(11))
const includeInternal = ref(false)
const loading = ref(false)
const message = ref<string | null>(null)
const report = ref<RevenueReport | null>(null)

/** Cents as a two-decimal amount; the currency is labelled beside it. */
function money(cents: number): string {
  return (cents / 100).toFixed(2)
}

async function load() {
  loading.value = true
  message.value = null
  try {
    report.value = await store.fetchRevenue({
      since: since.value || undefined,
      until: until.value || undefined,
      includeInternal: includeInternal.value,
    })
  } catch (e) {
    message.value = e instanceof Error ? e.message : 'revenue failed'
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
