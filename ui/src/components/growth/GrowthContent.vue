<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- FR-102 P7 — the Content table: per blog post, the web plane over the
       range (purestat, live), Google's clicks / impressions / CTR / position
       for the 7 and 28 days ending on the newest FINAL Search Console day,
       the index status from URL Inspection, and the sign-ups that landed on
       it. Three cells, never flattened: a value, a "not measured" chip with
       its reason, and a measured zero. Titles, paths and queries are
       rendered with {{ }}, never v-html. -->
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
      <div class="text-caption text-medium-emphasis mb-2">
        web {{ report.since }} → {{ report.until }} · Google
        <template v-if="report.google_through">
          7 / 28 days to {{ report.google_through }} (the newest final day)<template
            v-if="googleDaysShort(report)"
          >, {{ googleDaysShort(report) }}</template>
        </template>
        <template v-else>not measured</template>
        · excluded {{ report.excluded.users }} users, {{ report.excluded.tenants }} orgs
        <template v-if="report.include_internal"> · INTERNAL INCLUDED</template>
        · rates from n ≥ {{ report.n_floor }}
      </div>

      <div class="d-flex flex-wrap mb-3" style="gap: 6px">
        <growth-not-measured
          v-if="report.posts_not_measured"
          :reason="`posts: ${report.posts_not_measured.not_measured}`"
        />
        <growth-not-measured
          v-if="report.web_not_measured"
          :reason="`web plane: ${report.web_not_measured.not_measured}`"
        />
        <growth-not-measured
          v-if="report.google_not_measured"
          :reason="`Google: ${report.google_not_measured.not_measured}`"
        />
      </div>

      <v-table v-if="report.rows.length" density="compact" class="growth-wide">
        <thead>
          <tr>
            <th class="text-left">Post</th>
            <th class="text-left">Published</th>
            <th class="text-right">Visitors</th>
            <th class="text-right" title="pageviews visible ≥ 30 s">Reads</th>
            <th class="text-right">Median read</th>
            <th class="text-left">Top channels</th>
            <th class="text-right" title="clicks · impressions · CTR · position">Google 7d</th>
            <th class="text-right" title="clicks · impressions · CTR · position">Google 28d</th>
            <th class="text-left">Top queries (28d)</th>
            <th class="text-left">Indexed</th>
            <th class="text-right" :title="landingNote">Sign-ups (lower bound)</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="r in report.rows" :key="r.path" :class="{ 'text-medium-emphasis': !r.in_feed }">
            <td>
              <a :href="r.url" target="_blank" rel="noopener noreferrer">{{ r.title || r.path }}</a>
              <div class="text-caption text-medium-emphasis">
                {{ r.path }}<template v-if="!r.in_feed"> · no longer in the feed</template>
              </div>
            </td>
            <td>{{ r.published_at.slice(0, 10) }}</td>
            <template v-if="isNotMeasured(r.web)">
              <td colspan="4">
                <v-chip size="x-small" variant="tonal" color="grey" :title="r.web.not_measured">
                  not measured
                </v-chip>
              </td>
            </template>
            <template v-else>
              <td class="text-right">{{ r.web.visitors }}</td>
              <td class="text-right">
                {{ r.web.reads }}
                <div v-if="typeof r.web.read_share === 'number'" class="text-caption text-medium-emphasis">
                  {{ pct(r.web.read_share) }} of {{ r.web.pageviews }} views
                </div>
                <div v-else class="text-caption text-medium-emphasis">of {{ r.web.pageviews }} views</div>
              </td>
              <td class="text-right">{{ seconds(r.web.median_read_s) }}</td>
              <td>
                <span v-if="!r.web.channels.length" class="text-caption text-medium-emphasis">—</span>
                <div v-for="c in r.web.channels" :key="c.key" class="text-caption">
                  {{ c.key }} · {{ c.count }}
                </div>
              </td>
            </template>
            <template v-if="isNotMeasured(r.google)">
              <td colspan="3">
                <v-chip size="x-small" variant="tonal" color="grey" :title="r.google.not_measured">
                  not measured
                </v-chip>
              </td>
            </template>
            <template v-else>
              <td class="text-right">{{ searchLabel(r.google.d7) }}</td>
              <td class="text-right">{{ searchLabel(r.google.d28) }}</td>
              <td>
                <span v-if="!r.google.top_queries_28d.length" class="text-caption text-medium-emphasis">—</span>
                <div v-for="q in r.google.top_queries_28d" :key="q.key" class="text-caption">
                  {{ q.key }} · {{ q.clicks }}/{{ q.impressions }}
                </div>
              </td>
            </template>
            <td>
              <v-chip
                v-if="isNotMeasured(r.index)"
                size="x-small"
                variant="tonal"
                color="grey"
                :title="r.index.not_measured"
              >not measured</v-chip>
              <template v-else>
                <v-chip
                  v-if="r.index.inspected_at"
                  size="x-small"
                  variant="tonal"
                  :color="verdictColor(r.index.verdict)"
                >
                  {{ r.index.coverage_state || r.index.verdict || 'unknown' }}
                </v-chip>
                <v-chip v-else size="x-small" variant="tonal" color="grey" :title="r.index.error">
                  never inspected
                </v-chip>
                <div class="text-caption text-medium-emphasis">
                  <template v-if="r.index.last_crawl_time">crawled {{ r.index.last_crawl_time.slice(0, 10) }} · </template>
                  <template v-if="r.index.inspected_at">inspected {{ r.index.inspected_at.slice(0, 10) }}</template>
                  <template v-if="r.index.error">
                    <span :title="r.index.error">
                      · last attempt {{ (r.index.attempted_at || '').slice(0, 10) }} failed
                    </span>
                  </template>
                </div>
              </template>
            </td>
            <td class="text-right" :title="r.signups.note">{{ r.signups.count }}</td>
          </tr>
        </tbody>
      </v-table>
      <div v-if="report.rows.length" class="text-caption text-medium-emphasis mt-2">
        Sign-ups are a lower bound: {{ landingNote }}.
      </div>
      <v-alert v-else-if="!report.posts_not_measured" type="info" variant="tonal" density="compact">
        The blog's feed lists no post.
      </v-alert>
    </template>
    <div v-else-if="loading" class="pa-4"><v-progress-circular indeterminate size="24" /></div>
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useGrowthStore, isNotMeasured, type ContentReport, type SearchCell } from '@/stores/growth'
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
const includeInternal = ref(false)
const loading = ref(false)
const message = ref<string | null>(null)
const report = ref<ContentReport | null>(null)
const landingNote =
  'sign-ups whose first touch landed on the post — campaign links only until FR-91 P3 carries organic arrivals'

function pct(v: number): string {
  return `${Math.round(v * 1000) / 10}%`
}

function seconds(v: number | null | undefined): string {
  return typeof v === 'number' ? `${Math.round(v)} s` : '—'
}

/** `clicks / impressions`, then CTR and position once the server gives
 *  them (n ≥ 30 impressions). */
function searchLabel(c: SearchCell): string {
  let s = `${c.clicks} / ${c.impressions}`
  if (typeof c.ctr === 'number') s += ` · ${pct(c.ctr)}`
  if (typeof c.position === 'number') s += ` · #${c.position}`
  return s
}

/** "3 of 7 and 20 of 28 days pulled" when a Google window is short of
 *  final days — those days are uncounted, not zeros — else nothing. */
function googleDaysShort(r: ContentReport): string {
  const d = r.google_days
  if (!d || (d.d7 >= 7 && d.d28 >= 28)) return ''
  return `${d.d7} of 7 and ${d.d28} of 28 days pulled`
}

function verdictColor(v: string | null | undefined): string {
  if (v === 'PASS') return 'success'
  if (v === 'FAIL') return 'error'
  if (v === 'NEUTRAL') return 'warning'
  return 'grey'
}

async function load() {
  loading.value = true
  message.value = null
  try {
    report.value = await store.fetchContent({
      since: since.value || undefined,
      until: until.value || undefined,
      includeInternal: includeInternal.value,
    })
  } catch (e) {
    message.value = e instanceof Error ? e.message : 'content failed'
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
