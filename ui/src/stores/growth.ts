// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

// FR-102 P1d — the daily digest. Wire shapes mirror
// crates/api/src/growth/digest.rs. Three cells, never flattened: a measured
// value, a `{not_measured, arrives_with}` object, and a measured zero. The
// view renders the second as a muted chip and never as 0.

/** A count of the day, and of the trailing 7 and 28 days ending that day. */
export interface Figure {
  day: number
  d7: number
  d28: number
}

export interface NotMeasured {
  not_measured: string
  /** The phase that brings it (`P3`); absent when it needs configuration. */
  arrives_with?: string
}

export type Measured<T> = T | NotMeasured

export interface ExcludedCounts {
  users: number
  tenants: number
}

/** `preview` = a manual `send=false` result the scheduler will still claim. */
export type DigestStatus = 'computing' | 'sent' | 'stored' | 'failed' | 'preview'

export interface Digest {
  day: string
  status: DigestStatus
  version: number
  /** RFC 3339 — the server renders dates as strings, never `{$date}`. */
  claimed_at: string
  computed_at?: string
  sent_at?: string
  attempts: number
  error?: string
  /** A COUNT of recipients, never the addresses. */
  recipients: number
  excluded: ExcludedCounts
  sections?: Record<string, unknown>
}

export interface DigestListRow {
  day: string
  status: DigestStatus
  computed_at?: string
  sent_at?: string
  attempts: number
  recipients: number
  error?: string
  excluded: ExcludedCounts
  headline?: Record<string, unknown>
}

export interface DigestPreview {
  day: string
  include_internal: boolean
  excluded: ExcludedCounts
  sections: Record<string, unknown>
}

// ── P2: the account funnel, cohorts and the org views ───────────────────
// Wire shapes mirror crates/api/src/growth/{funnel,cohorts,orgs}.rs. The
// n-floor rule (SMALL_N = 30) lives in `rateIfEnough`: below its settled
// denominator a step shows counts, never a percentage.

/** A windowed step: `settled = signups − pending` is the rate denominator. */
export interface StepCounts {
  count: number
  pending: number
  settled: number
}

export interface FunnelSteps {
  signup: number
  org_created: StepCounts
  /** `null` without `fleet` — "cannot see devices", never 0. */
  activated: StepCounts | null
  first_value: StepCounts | null
  /** P4 — the org's first invoice paid above 0; no window, so "paid so
   *  far". `null` without `saas` (see `not_measured.paid`). */
  paid: StepCounts | null
}

export interface Percentiles {
  median_h: number | null
  p75_h: number | null
  n: number
}

export interface TimeToValue {
  first_device: Percentiles | null
  first_value: Percentiles | null
}

export interface FunnelBucket {
  key: string | null
  signups: number
  org_created: StepCounts
  activated: StepCounts | null
  first_value: StepCounts | null
  paid: StepCounts | null
}

export interface FunnelWindows {
  org_days: number
  activation_days: number
  value_days: number
}

export type FunnelDimension =
  | 'source'
  | 'medium'
  | 'campaign'
  | 'content'
  | 'channel'
  | 'self_reported'
  | 'landing_path'

export interface FunnelReport {
  enabled: boolean
  since: string
  until: string
  include_internal: boolean
  excluded: ExcludedCounts
  windows: FunnelWindows
  truncated: boolean
  value_basis: string[]
  not_measured: Record<string, string>
  steps: FunnelSteps
  time_to_value: TimeToValue
  by: FunnelDimension | null
  buckets: FunnelBucket[]
}

export interface CohortRow {
  week: string
  start: string
  size: number
  /** `active[k]` = orgs active in W+k; `null` until that week has started. */
  active: (number | null)[]
  closed_through: number
}

export interface WaoWeek {
  week: string
  start: string
  active_orgs: number
  partial: boolean
}

export interface CohortsReport {
  enabled: boolean
  unit: string
  since: string
  include_internal: boolean
  excluded: ExcludedCounts
  weeks_since: number
  highlight: number[]
  value_basis: string[]
  not_measured?: string
  cohorts: CohortRow[] | null
  wao: WaoWeek[] | null
  org_daily: { first_day: string | null; last_day: string | null }
}

export interface Acquisition {
  channel: string
  source?: string
  medium?: string
  campaign?: string
  content?: string
  self_reported?: string
  landing_path?: string
  referrer_host?: string
}

export interface OrgRow {
  tenant_id: string
  name: string
  slug: string
  created: string
  archived: boolean
  plan: string
  billing_status?: string
  owner: Acquisition | null
  devices: { enrolled: number; live: number } | null
  last_value_day: string | null
  weeks: boolean[]
  active_weeks: number
  at_risk: boolean
}

export interface OrgsReport {
  enabled: boolean
  include_internal: boolean
  excluded: ExcludedCounts
  limit: number
  truncated: boolean
  value_basis: string[]
  not_measured?: string
  weeks: string[]
  orgs: OrgRow[]
}

export interface TimelineEvent {
  at: string
  kind: string
  attribution?: Acquisition
  owner?: boolean
  os?: string
  value_kinds?: string[]
}

export interface OrgTimeline {
  enabled: boolean
  tenant_id: string
  name: string
  slug: string
  created: string
  archived: boolean
  plan: string
  billing_status?: string
  members: number
  devices: { enrolled: number; live: number } | null
  events: TimelineEvent[]
  value_basis: string[]
  not_measured?: string
  weeks: { week: string; start: string; active: boolean }[]
  totals: Record<string, unknown> | null
  /** P4 — plan changes and invoices, oldest first; not measured without `saas`. */
  plan_history: Measured<PlanChange[]>
}

/** P4 — one row of an org's billing history: what Stripe said. */
export interface PlanChange {
  at: string
  /** `subscription_created` · `subscription_updated` · `subscription_deleted`
   *  · `invoice_paid` · `invoice_payment_failed` · `trial_will_end`. */
  kind: string
  plan?: string
  seats?: number
  /** Invoices only: what was billed, in cents. */
  amount_cents?: number
  currency?: string
  status?: string
}

// ── P4: revenue ─────────────────────────────────────────────────────────
// Wire shapes mirror crates/api/src/growth/revenue.rs. Billed figures are
// what Stripe billed (`billing_events`); the list-price figure is an
// ESTIMATE and says so. A month before the first billing row is
// `on_record: false` — not measured, never a month of zero revenue.

export interface CurrencyNow {
  /** Lowercase ISO 4217, as Stripe spells it. */
  currency: string
  mrr_cents: number
  paying_orgs: number
}

/** One currency's movements in one month (MRR at the month's end). */
export interface CurrencyMonth {
  currency: string
  month: string
  mrr_cents: number
  paying_orgs: number
  new_cents: number
  new_orgs: number
  expansion_cents: number
  contraction_cents: number
  churned_cents: number
  churned_orgs: number
  net_new_cents: number
  invoices_paid: number
  billed_cents: number
}

export interface MonthBlock {
  month: string
  /** Read from the frozen monthly totals, not computed live. */
  frozen: boolean
  /** The open month: its "end" is now. */
  partial: boolean
  /** `false` = before the first billing row: not measured. */
  on_record: boolean
  currencies: CurrencyMonth[]
}

export interface RevenueRecord {
  since: string
  last_event_at?: string
  /** Absent = the backfill never ran: figures start at the webhook. */
  backfilled_at?: string
  rows: number
  truncated: boolean
}

export interface ChannelRevenue {
  channel: string
  currency: string
  paying_orgs: number
  mrr_cents: number
  new_paying_orgs: number
  churned_orgs: number
  trials: number
  converted: number
}

export interface BilledRevenue {
  record: RevenueRecord
  now: CurrencyNow[]
  months: MonthBlock[]
  totals: {
    invoices_paid: number
    billed: { currency: string; cents: number }[]
    payment_failures: number
    new_paying_orgs: number
    cancellations: number
  }
  trial_to_paid: { trials: number; converted: number; rate: Rate }
  by_channel: ChannelRevenue[]
}

export interface OrgRevenue {
  tenant_id: string
  name: string
  deleted: boolean
  plan: string
  billing_status?: string
  seats: number
  channel: string | null
  paid_since: string | null
  /** `null` = no paid invoice on record. */
  billed_mrr_cents: number | null
  currency: string | null
  /** The list-price ESTIMATE (plan price × seats). */
  list_price_mrr_cents: number
  /** In `cost.currency`; `null` = not priced or not measured. */
  cost_30d: number | null
  /** Billed MRR − cost, cents; only where both exist in one currency. */
  margin_cents: number | null
  collisions: number
}

export interface CollisionRow {
  tenant_id: string
  name: string
  plan: string
  total: number
  days: number
  last_day: string
  limits: { limit: string; count: number }[]
}

export interface RevenueReport {
  enabled: boolean
  since: string
  until: string
  include_internal: boolean
  excluded: ExcludedCounts
  as_of: string
  billed: Measured<BilledRevenue>
  estimate: { cents: number; estimate: boolean; basis: string }
  cost: Measured<{ range: string; currency?: string; priced: boolean }>
  upgrade_intent: {
    collisions: number
    orgs: number
    by_limit: { limit: string; count: number }[]
    top: CollisionRow[]
  }
  orgs: OrgRevenue[]
  orgs_truncated: boolean
}

// ── P6: the Campaigns table ─────────────────────────────────────────────
// Wire shapes mirror crates/api/src/growth/campaigns.rs. Every key field is
// client-chosen text (anyone can craft a UTM link): render it as text,
// never as markup.

/** How deep the key goes: each depth adds one UTM field. */
export type KeyDepth = 'source' | 'medium' | 'campaign' | 'content'

/** The normalised key; a field below the depth (or absent) is `null`. */
export interface CampaignKey {
  source: string
  medium: string | null
  campaign: string | null
  content: string | null
}

export interface CampaignGoals {
  signup: number
  subscribe: number
  install_copy: number
  github_outbound: number
}

export interface WebCells {
  visitors: number
  visits: number
  pageviews: number
  /** Pageviews visible ≥ 30 s (purestat's `reads`). */
  reads: number
  goals: Measured<CampaignGoals>
  /** purestat rows folded into the key; above 1 the counts are sums. */
  spellings: number
}

/** A rate and its n; `value` only from the n-floor up. */
export interface Rate {
  n: number
  value?: number
}

export interface Coverage {
  purestat_signup_goals: number
  server_signups: number
  ratio?: number
  n_floor: number
}

export interface Rates {
  signup_per_visitor: Rate | null
  activation: Rate | null
  first_value: Rate | null
  /** P4 — paid (so far) ÷ the sign-ups settled at the org step. */
  paid: Rate | null
}

export interface CampaignRow {
  key: CampaignKey
  /** The taxonomy's group of the source: canonical, or `other`. */
  group: string
  web: Measured<WebCells>
  signups: number
  org_created: StepCounts
  activated: StepCounts | null
  first_value: StepCounts | null
  paid: StepCounts | null
  /** P8 — each platform's own numbers for the key's posts in the range. */
  reach: Measured<PlatformReach[]>
  coverage: Measured<Coverage>
  rates: Rates
}

export interface CampaignTotals {
  /** One purestat query with no dimension: unique over the range. */
  web: Measured<WebCells>
  signups: number
  keyed_signups: number
  coverage: Measured<Coverage>
}

/** The daily puller's state over the range (`growth_pulls`). */
export interface PullSummary {
  source: string
  days: number
  pulled: number
  failed: number
  not_pulled: number
  freeze_days: number
  last_pulled_at?: string
  last_error?: string
  not_measured?: string
}

export interface CampaignsReport {
  enabled: boolean
  since: string
  until: string
  by: KeyDepth
  include_internal: boolean
  excluded: ExcludedCounts
  windows: FunnelWindows
  n_floor: number
  /** Why every web cell is not measured (no key, 401, timeout…). */
  web_not_measured?: NotMeasured
  /** Why no web cell carries conversions (a goal the site lacks). */
  conversions?: NotMeasured
  web_truncated: boolean
  account_truncated: boolean
  rows_truncated: boolean
  value_basis: string[]
  not_measured?: Record<string, string>
  totals: CampaignTotals
  rows: CampaignRow[]
  pulls: PullSummary
}

// ── P8: the Channels table and the ingest keys ─────────────────────────
// Wire shapes mirror crates/api/src/growth/{channels,keys}.rs. Metric names
// are the platforms' own (raw) — a metric is only ever compared with itself.

/** A platform's own numbers at one snapshot age, summed over its posts. */
export interface ReachAt {
  snapshot: string
  posts: number
  metrics: Record<string, number>
}

export interface PlatformReach {
  platform: string
  posts: number
  at: ReachAt[]
}

export interface SnapshotOut {
  snapshot: string
  taken_at: string
  metrics: Record<string, number>
}

/** A cost per unit; `cents` only from n = 30. */
export interface CostRate {
  n: number
  cents?: number
}

export interface Spend {
  cost_cents: number
  posts_with_cost: number
  per_visitor: CostRate | null
  per_signup: CostRate
}

export interface PostRow {
  platform: string
  platform_id: string
  account: string
  format: string
  url: string
  published_at: string
  key: CampaignKey
  cost_cents?: number
  snapshots: SnapshotOut[]
  reach: Measured<SnapshotOut>
  web: Measured<WebCells>
  signups: number
  org_created: StepCounts
  activated: StepCounts | null
  first_value: StepCounts | null
  paid: StepCounts | null
  rates: Rates
  spend?: Spend
}

export interface PlatformSummary {
  posts: number
  reach: Measured<ReachAt[]>
  /** Σ over the posts' distinct keys — an upper bound on unique visitors. */
  web: Measured<WebCells>
  signups: number
  org_created: StepCounts
  activated: StepCounts | null
  first_value: StepCounts | null
  paid: StepCounts | null
  rates: Rates
  spend?: Spend
}

export interface PlatformRow {
  platform: string
  retired: boolean
  summary: Measured<PlatformSummary>
  posts: PostRow[]
}

export interface ChannelsReport {
  enabled: boolean
  since: string
  until: string
  include_internal: boolean
  excluded: ExcludedCounts
  n_floor: number
  ingest_mounted: boolean
  web_not_measured?: NotMeasured
  conversions?: NotMeasured
  web_truncated: boolean
  account_truncated: boolean
  posts_truncated: boolean
  value_basis: string[]
  not_measured?: Record<string, string>
  platforms: PlatformRow[]
}

export interface GrowthKeyRow {
  id: string
  kind: 'ingest'
  label: string
  /** The key's first characters — to recognise it, never to use it. */
  prefix: string
  created_by: string
  created_at: string
  last_used_at?: string
  revoked_at?: string
  live: boolean
}

export interface KeysResponse {
  enabled: boolean
  ingest_mounted: boolean
  keys: GrowthKeyRow[]
}

/** The mint answer: the only place the key itself ever appears. */
export interface MintedKey extends GrowthKeyRow {
  key: string
}

// ── P5: the newsletter ─────────────────────────────────────────────────
// Wire shapes mirror crates/api/src/growth/newsletter.rs. Delivery is
// SendGrid's signed Event Webhook (delivered, bounces, drops, spam reports —
// never opens or clicks); `not_measured` when this server does not verify
// it, never 0. Nothing here names a person: every number is a count.

export interface NewsletterListDay {
  day: string
  new: number
  confirmed: number
  unsubscribed: number
  suppressed: number
}

export interface NewsletterListSize {
  /** Confirmed, not withdrawn, not suppressed — who the next issue mails. */
  mailable: number
  pending: number
  unsubscribed: number
  /** Confirmed but suppressed (a hard bounce or a spam report). */
  suppressed: number
}

export interface NewsletterListTotals {
  new: number
  confirmed: number
  unsubscribed: number
  /** Of `unsubscribed`, through an issue's own link. */
  unsubscribed_via_issue: number
  suppressed: number
}

export interface NewsletterListSource {
  source: string
  new: number
  confirmed: number
  unsubscribed: number
}

export interface NewsletterList {
  size: NewsletterListSize
  totals: NewsletterListTotals
  days: NewsletterListDay[]
  by_source: NewsletterListSource[]
}

/** The send ledger: accepted means "accepted by SendGrid", not delivered. */
export interface LedgerCounts {
  recipients: number
  accepted: number
  failed: number
  suppressed: number
  stale: number
}

export interface DeliveryCounts {
  delivered: number
  /** Every bounce, hard and blocked. */
  bounced: number
  hard_bounced: number
  dropped: number
  /** Spam reports. */
  complaints: number
}

export interface IssueRates {
  delivered: Rate | null
  hard_bounce: Rate | null
  complaint: Rate | null
  unsubscribe: Rate
  signup_per_visitor: Rate | null
}

export interface NewsletterIssueRow {
  slug: string
  subject: string
  status: string
  sent_at?: string
  /** The `utm_campaign` its links carried; absent = untagged. */
  campaign?: string
  /** A test issue: the size of the audience it was restricted to. */
  test_audience?: number
  ledger: LedgerCounts
  delivery: Measured<DeliveryCounts>
  unsubscribed_via: number
  web: Measured<WebCells>
  signups: number
  rates: IssueRates
}

export interface NewsletterReport {
  enabled: boolean
  since: string
  until: string
  include_internal: boolean
  excluded: ExcludedCounts
  n_floor: number
  /** Whether this server verifies SendGrid's Event Webhook. */
  webhook_configured: boolean
  link_tags: boolean
  /** The whole view's reason (the saas module not mounted). */
  not_measured?: NotMeasured
  list?: NewsletterList
  issues: NewsletterIssueRow[]
  issues_truncated: boolean
  web_not_measured?: NotMeasured
  web_truncated: boolean
  account_truncated: boolean
}

/** Cents as a two-decimal amount in the account's currency. */
export function centsLabel(cents: number): string {
  return (cents / 100).toFixed(2)
}

/** `impressions 3000 · clicks 80` — the first `max` metrics of an age,
 *  raw names, as text. */
export function metricsLabel(metrics: Record<string, number>, max = 3): string {
  const parts = Object.entries(metrics)
    .slice(0, max)
    .map(([k, v]) => `${k} ${Number.isInteger(v) ? v : v.toFixed(1)}`)
  const more = Object.keys(metrics).length - parts.length
  return more > 0 ? `${parts.join(' · ')} · +${more}` : parts.join(' · ')
}

/** The oldest age a reach has — the platform's latest word. */
export function latestReach(at: ReachAt[]): ReachAt | null {
  return at.length ? at[at.length - 1] : null
}

/** Only an http(s) URL is ever rendered as a link. */
export function safeHref(url: string): string | null {
  return /^https?:\/\//i.test(url) ? url : null
}

/** `linkedin / post / 20261019-x` — the key at its depth, absent fields
 *  left out. Plain text: the caller interpolates it, never `v-html`. */
export function campaignKeyLabel(k: CampaignKey): string {
  return [k.source, k.medium, k.campaign, k.content]
    .filter((v): v is string => typeof v === 'string' && v.length > 0)
    .join(' / ')
}

/** A server-side rate as `12.3%`; `null` below the n-floor (the server
 *  leaves `value` out) or when the rate does not exist. */
export function rateLabel(r: Rate | null | undefined): string | null {
  if (!r || typeof r.value !== 'number') return null
  return `${Math.round(r.value * 1000) / 10}%`
}

/** The n-floor (spec principle 5): a rate only once its denominator reaches
 *  30 — below it the UI shows counts and hides the percentage. */
export const SMALL_N = 30

/** `count / settled` as a percentage to 0.1, or `null` below the n-floor. */
export function rateIfEnough(count: number, settled: number): number | null {
  if (settled < SMALL_N) return null
  return Math.round((count / settled) * 1000) / 10
}

export function isNotMeasured(v: unknown): v is NotMeasured {
  return (
    typeof v === 'object' &&
    v !== null &&
    typeof (v as NotMeasured).not_measured === 'string'
  )
}

export function isFigure(v: unknown): v is Figure {
  if (typeof v !== 'object' || v === null) return false
  const f = v as Record<string, unknown>
  return typeof f.day === 'number' && typeof f.d7 === 'number' && typeof f.d28 === 'number'
}

/** `not measured — arrives with P3`, or the reason when no phase brings it. */
export function notMeasuredLabel(n: NotMeasured): string {
  return n.arrives_with
    ? `not measured — arrives with ${n.arrives_with}`
    : `not measured — ${n.not_measured}`
}

/** `12 (7d 80 · 28d 300)` — the day beside its context, always. */
export function figureLabel(f: Figure): string {
  return `${f.day} (7d ${f.d7} · 28d ${f.d28})`
}

/** `orgs_devices` → `Orgs devices`; a wire key as a heading. */
export function humanKey(key: string): string {
  const s = key.replace(/_/g, ' ')
  return s.charAt(0).toUpperCase() + s.slice(1)
}

export const useGrowthStore = defineStore('growth', () => {
  const digests = ref<DigestListRow[]>([])
  const error = ref<string | null>(null)

  async function fetchDigests(since?: string, limit = 30): Promise<DigestListRow[]> {
    const q = new URLSearchParams()
    if (since) q.set('since', since)
    q.set('limit', String(limit))
    try {
      const r = await api.get<{ digests: DigestListRow[] }>(`/admin/growth/digests?${q}`)
      digests.value = r.digests ?? []
      error.value = null
    } catch (e) {
      error.value = e instanceof Error ? e.message : 'digests failed'
      digests.value = []
    }
    return digests.value
  }

  /** A stored day; the latest when `day` is omitted; `null` when there is none. */
  async function fetchDigest(day?: string): Promise<Digest | null> {
    const q = day ? `?day=${encodeURIComponent(day)}` : ''
    try {
      return await api.get<Digest>(`/admin/growth/digest${q}`)
    } catch (e) {
      if ((e as { status?: number }).status === 404) return null
      throw e
    }
  }

  /** Recompute a day. Never mails unless `send` — and never a `sent` day. */
  async function runDigest(day: string, send = false): Promise<Digest> {
    const q = new URLSearchParams({ day, send: String(send) })
    return api.post<Digest>(`/admin/growth/digest/run?${q}`)
  }

  /** Compute live, store nothing; `includeInternal` is the raw totals. */
  async function fetchPreview(day: string, includeInternal: boolean): Promise<DigestPreview> {
    const q = new URLSearchParams({ day, include_internal: String(includeInternal) })
    return api.get<DigestPreview>(`/admin/growth/digest/preview?${q}`)
  }

  /** The account funnel; `by` adds a per-dimension breakdown. */
  async function fetchFunnel(
    opts: { since?: string; until?: string; by?: FunnelDimension; includeInternal?: boolean } = {},
  ): Promise<FunnelReport> {
    const q = new URLSearchParams()
    if (opts.since) q.set('since', opts.since)
    if (opts.until) q.set('until', opts.until)
    if (opts.by) q.set('by', opts.by)
    if (opts.includeInternal) q.set('include_internal', 'true')
    const qs = q.toString()
    return api.get<FunnelReport>(`/admin/growth/funnel${qs ? `?${qs}` : ''}`)
  }

  /** Weekly cohorts and the north-star WAO series. */
  async function fetchCohorts(since?: string): Promise<CohortsReport> {
    const q = new URLSearchParams({ unit: 'week' })
    if (since) q.set('since', since)
    return api.get<CohortsReport>(`/admin/growth/cohorts?${q}`)
  }

  /** The per-org table. */
  async function fetchOrgs(limit?: number): Promise<OrgsReport> {
    const q = new URLSearchParams()
    if (limit) q.set('limit', String(limit))
    const qs = q.toString()
    return api.get<OrgsReport>(`/admin/growth/orgs${qs ? `?${qs}` : ''}`)
  }

  /** One org's milestones from sign-up to its last value day. */
  async function fetchTimeline(tenantId: string): Promise<OrgTimeline> {
    return api.get<OrgTimeline>(`/admin/growth/org/${encodeURIComponent(tenantId)}/timeline`)
  }

  /** P4 — billed revenue: months are `YYYY-MM`, both inclusive. */
  async function fetchRevenue(
    opts: { since?: string; until?: string; includeInternal?: boolean } = {},
  ): Promise<RevenueReport> {
    const q = new URLSearchParams()
    if (opts.since) q.set('since', opts.since)
    if (opts.until) q.set('until', opts.until)
    if (opts.includeInternal) q.set('include_internal', 'true')
    const qs = q.toString()
    return api.get<RevenueReport>(`/admin/growth/revenue${qs ? `?${qs}` : ''}`)
  }

  /** P6 — the Campaigns table: days are `YYYY-MM-DD`, both inclusive. */
  async function fetchCampaigns(
    opts: { since?: string; until?: string; by?: KeyDepth; includeInternal?: boolean } = {},
  ): Promise<CampaignsReport> {
    const q = new URLSearchParams()
    if (opts.since) q.set('since', opts.since)
    if (opts.until) q.set('until', opts.until)
    if (opts.by) q.set('by', opts.by)
    if (opts.includeInternal) q.set('include_internal', 'true')
    const qs = q.toString()
    return api.get<CampaignsReport>(`/admin/growth/campaigns${qs ? `?${qs}` : ''}`)
  }

  /** P8 — the Channels table: posts published in the range, per platform. */
  async function fetchChannels(
    opts: { since?: string; until?: string; includeInternal?: boolean } = {},
  ): Promise<ChannelsReport> {
    const q = new URLSearchParams()
    if (opts.since) q.set('since', opts.since)
    if (opts.until) q.set('until', opts.until)
    if (opts.includeInternal) q.set('include_internal', 'true')
    const qs = q.toString()
    return api.get<ChannelsReport>(`/admin/growth/channels${qs ? `?${qs}` : ''}`)
  }

  /** P5 — the newsletter: the list over time and the per-issue table. */
  async function fetchNewsletter(
    opts: { since?: string; until?: string; includeInternal?: boolean } = {},
  ): Promise<NewsletterReport> {
    const q = new URLSearchParams()
    if (opts.since) q.set('since', opts.since)
    if (opts.until) q.set('until', opts.until)
    if (opts.includeInternal) q.set('include_internal', 'true')
    const qs = q.toString()
    return api.get<NewsletterReport>(`/admin/growth/newsletter${qs ? `?${qs}` : ''}`)
  }

  /** P8 — the growth machine keys (never the key or its hash). */
  async function fetchKeys(): Promise<KeysResponse> {
    return api.get<KeysResponse>('/admin/growth/keys')
  }

  /** P8 — mint an ingest key; the answer is the only place it appears. */
  async function mintKey(label: string): Promise<MintedKey> {
    return api.post<MintedKey>('/admin/growth/keys', { kind: 'ingest', label })
  }

  async function revokeKey(id: string): Promise<void> {
    await api.delete(`/admin/growth/keys/${encodeURIComponent(id)}`)
  }

  return {
    digests,
    error,
    fetchDigests,
    fetchDigest,
    runDigest,
    fetchPreview,
    fetchFunnel,
    fetchCohorts,
    fetchOrgs,
    fetchTimeline,
    fetchRevenue,
    fetchCampaigns,
    fetchChannels,
    fetchNewsletter,
    fetchKeys,
    mintKey,
    revokeKey,
  }
})
