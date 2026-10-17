// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-102 P1d — the growth store: the three cells stay three (a measured
// value, `not_measured`, a measured zero), the day keeps its 7/28-day
// context, and the four calls hit the routes the server mounts.
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

import {
  useGrowthStore,
  isNotMeasured,
  isFigure,
  notMeasuredLabel,
  figureLabel,
  humanKey,
  rateIfEnough,
  SMALL_N,
  campaignKeyLabel,
  rateLabel,
  centsLabel,
  metricsLabel,
  latestReach,
  safeHref,
  weekFigureLabel,
  isWeekFigure,
  alertDetailLabel,
  msLabel,
  pooledLabel,
} from '@/stores/growth'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

describe('growth helpers', () => {
  it('tells not-measured from a measured zero', () => {
    expect(isNotMeasured({ not_measured: 'purestat key not configured' })).toBe(true)
    expect(isNotMeasured({ day: 0, d7: 0, d28: 0 })).toBe(false)
    expect(isNotMeasured(0)).toBe(false)
    expect(isNotMeasured(null)).toBe(false)
    expect(isFigure({ day: 0, d7: 0, d28: 0 })).toBe(true)
    expect(isFigure({ day: 1 })).toBe(false)
    expect(isFigure({ not_measured: 'x' })).toBe(false)
  })

  it('labels a not-measured cell with its phase, never a number', () => {
    expect(notMeasuredLabel({ not_measured: 'install', arrives_with: 'P3' })).toBe(
      'not measured — arrives with P3',
    )
    expect(notMeasuredLabel({ not_measured: 'purestat key not configured' })).toBe(
      'not measured — purestat key not configured',
    )
    expect(figureLabel({ day: 2, d7: 9, d28: 31 })).toBe('2 (7d 9 · 28d 31)')
    expect(humanKey('orgs_devices')).toBe('Orgs devices')
  })

  it('labels a campaign key by its present fields, as plain text', () => {
    // P6 — a key cut to its depth has null fields; they are left out.
    expect(
      campaignKeyLabel({ source: 'linkedin', medium: 'post', campaign: '20261019-x', content: null }),
    ).toBe('linkedin / post / 20261019-x')
    expect(campaignKeyLabel({ source: 'youtube', medium: null, campaign: null, content: null })).toBe(
      'youtube',
    )
    // A value is never interpreted: markup stays text in the label.
    expect(
      campaignKeyLabel({ source: '<b>x</b>', medium: null, campaign: null, content: null }),
    ).toBe('<b>x</b>')
  })

  it('shows a server rate only when it carries a value (n ≥ 30)', () => {
    // P6 — below the n-floor the server leaves `value` out.
    expect(rateLabel({ n: 29 })).toBeNull()
    expect(rateLabel(null)).toBeNull()
    expect(rateLabel(undefined)).toBeNull()
    expect(rateLabel({ n: 55, value: 0.018 })).toBe('1.8%')
    expect(rateLabel({ n: 30, value: 0 })).toBe('0%')
  })

  it('labels reach with raw metric names and links only http(s) URLs', () => {
    // P8 — a platform's own metric names, verbatim; the oldest age is the latest word.
    expect(metricsLabel({ impressions: 3000, clicks: 80 })).toBe('impressions 3000 · clicks 80')
    expect(metricsLabel({ a: 1, b: 2, c: 3, d: 4 }, 2)).toBe('a 1 · b 2 · +2')
    expect(metricsLabel({ averageViewPercentage: 41.54 })).toBe('averageViewPercentage 41.5')
    expect(latestReach([])).toBeNull()
    expect(
      latestReach([
        { snapshot: 'T+1d', posts: 1, metrics: {} },
        { snapshot: 'T+7d', posts: 1, metrics: {} },
      ])?.snapshot,
    ).toBe('T+7d')
    expect(centsLabel(5000)).toBe('50.00')
    expect(centsLabel(9091)).toBe('90.91')
    expect(safeHref('https://www.linkedin.com/feed/update/1/')).toBe(
      'https://www.linkedin.com/feed/update/1/',
    )
    expect(safeHref('javascript:alert(1)')).toBeNull()
    expect(safeHref('//evil.example')).toBeNull()
  })

  it('labels a week figure against its baselines, and an alert detail as text', () => {
    // P10 — the week, the week before, the four-week mean when measurable.
    expect(weekFigureLabel({ week: 12, prev_week: 9, w4_avg: 5 })).toBe('12 (prev 9 · 4w avg 5.0)')
    expect(weekFigureLabel({ week: 300, prev_week: 280 })).toBe('300 (prev 280)')
    expect(isWeekFigure({ week: 1, prev_week: 0 })).toBe(true)
    expect(isWeekFigure({ day: 1, d7: 2, d28: 3 })).toBe(false)
    expect(isWeekFigure({ not_measured: 'fleet module not mounted' })).toBe(false)
    // Detail values are text, never interpreted; a nested object is JSON.
    expect(alertDetailLabel({ day: '2026-10-14', value: 9, baseline_mean: 1.07 })).toBe(
      'day 2026-10-14 · value 9 · baseline_mean 1.07',
    )
    expect(alertDetailLabel({ days: { '2026-10-13': 'failed' }, '<b>x</b>': '<i>y</i>' })).toBe(
      'days {"2026-10-13":"failed"} · <b>x</b> <i>y</i>',
    )
  })

  it('labels milliseconds as a reader reads them, and a pooled percentile as an estimate', () => {
    // P9a — the server's own labelling; `—` for nothing, never 0.
    expect(msLabel(820)).toBe('820 ms')
    expect(msLabel(2736)).toBe('2.7 s')
    expect(msLabel(738_000)).toBe('12.3 min')
    expect(msLabel(5_400_000)).toBe('1.5 h')
    expect(msLabel(null)).toBe('—')
    expect(msLabel(undefined)).toBe('—')
    // One day's values are exact; several days' are pooled from the daily
    // histograms and say so; no sample is not a zero.
    expect(pooledLabel({ n: 94, p50: 2736, p90: 6100, exact: true, parts: 1 })).toBe(
      'p50 2.7 s · p90 6.1 s (n 94)',
    )
    expect(pooledLabel({ n: 300, p50: 2500, p90: 7000, exact: false, parts: 7 })).toBe(
      '≈ p50 2.5 s · p90 7.0 s (n 300)',
    )
    expect(pooledLabel({ n: 0, exact: true, parts: 1 })).toBe('no sample')
    expect(pooledLabel({ n: 3, p50: 30, exact: true, parts: 1 }, (v) => `${v} fps`)).toBe(
      'p50 30 fps · p90 — (n 3)',
    )
  })

  it('shows a rate only once the settled denominator reaches the n-floor', () => {
    // P2 — below SMALL_N the UI shows counts and hides the percentage.
    expect(rateIfEnough(5, SMALL_N - 1)).toBeNull()
    expect(rateIfEnough(0, 0)).toBeNull()
    expect(rateIfEnough(15, 30)).toBe(50)
    expect(rateIfEnough(1, 300)).toBe(0.3)
    expect(SMALL_N).toBe(30)
  })
})

describe('useGrowthStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  it('lists digests from the admin route and keeps them', async () => {
    mockApi.get.mockResolvedValueOnce({
      enabled: true,
      digests: [{ day: '2026-10-13', status: 'stored', attempts: 1, recipients: 0, excluded: { users: 1, tenants: 2 } }],
    })
    const store = useGrowthStore()
    const rows = await store.fetchDigests(undefined, 10)
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/digests?limit=10')
    expect(rows).toHaveLength(1)
    expect(store.digests[0].day).toBe('2026-10-13')
    expect(store.error).toBeNull()
  })

  it('a missing day is null, not an error; other failures throw', async () => {
    mockApi.get.mockRejectedValueOnce(Object.assign(new Error('nf'), { status: 404 }))
    const store = useGrowthStore()
    expect(await store.fetchDigest('2026-10-13')).toBeNull()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/digest?day=2026-10-13')
    mockApi.get.mockRejectedValueOnce(Object.assign(new Error('boom'), { status: 500 }))
    await expect(store.fetchDigest()).rejects.toThrow('boom')
    expect(mockApi.get).toHaveBeenLastCalledWith('/admin/growth/digest')
  })

  it('recompute never asks to send unless told, and the preview carries include_internal', async () => {
    mockApi.post.mockResolvedValueOnce({ day: '2026-10-13', status: 'stored' })
    mockApi.get.mockResolvedValueOnce({ day: '2026-10-13', include_internal: true, sections: {} })
    const store = useGrowthStore()
    await store.runDigest('2026-10-13')
    expect(mockApi.post).toHaveBeenCalledWith('/admin/growth/digest/run?day=2026-10-13&send=false')
    await store.fetchPreview('2026-10-13', true)
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/digest/preview?day=2026-10-13&include_internal=true',
    )
  })

  it('fetches the funnel, cohorts, orgs and a timeline at the P2 routes', async () => {
    const store = useGrowthStore()
    mockApi.get.mockResolvedValue({})
    await store.fetchFunnel()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/funnel')
    await store.fetchFunnel({ by: 'source', includeInternal: true })
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/funnel?by=source&include_internal=true',
    )
    await store.fetchCohorts()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/cohorts?unit=week')
    await store.fetchOrgs(50)
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/orgs?limit=50')
    await store.fetchTimeline('69a1dbbad2000f26adc875ce')
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/org/69a1dbbad2000f26adc875ce/timeline',
    )
  })

  it('fetches remote-desktop quality at the P9a route with its day range', async () => {
    const store = useGrowthStore()
    mockApi.get.mockResolvedValue({})
    await store.fetchQuality()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/quality')
    await store.fetchQuality({ since: '2026-09-18', until: '2026-10-15', includeInternal: true })
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/quality?since=2026-09-18&until=2026-10-15&include_internal=true',
    )
  })

  it('fetches billed revenue at the P4 route with its month range', async () => {
    const store = useGrowthStore()
    mockApi.get.mockResolvedValue({})
    await store.fetchRevenue()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/revenue')
    await store.fetchRevenue({ since: '2025-11', until: '2026-10', includeInternal: true })
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/revenue?since=2025-11&until=2026-10&include_internal=true',
    )
  })

  it('reads billed revenue with nothing on record as not measured, never 0 MRR', () => {
    // The two shapes the billed cell takes.
    expect(
      isNotMeasured({ not_measured: 'no billing event on record yet' }),
    ).toBe(true)
    expect(
      isNotMeasured({ record: { since: '2026-10-01', rows: 1, truncated: false }, now: [] }),
    ).toBe(false)
  })

  it('fetches the campaigns table at the P6 route with its range and depth', async () => {
    const store = useGrowthStore()
    mockApi.get.mockResolvedValue({})
    await store.fetchCampaigns()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/campaigns')
    await store.fetchCampaigns({
      since: '2026-10-01',
      until: '2026-10-13',
      by: 'content',
      includeInternal: true,
    })
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/campaigns?since=2026-10-01&until=2026-10-13&by=content&include_internal=true',
    )
  })

  it('fetches the content table at the P7 route with its range', async () => {
    const store = useGrowthStore()
    mockApi.get.mockResolvedValue({ rows: [] })
    await store.fetchContent()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/content')
    await store.fetchContent({ since: '2026-10-01', until: '2026-10-13', includeInternal: true })
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/content?since=2026-10-01&until=2026-10-13&include_internal=true',
    )
    // A post's cells keep the three states apart: a value, a reason, a zero.
    expect(isNotMeasured({ not_measured: 'purestat key not configured' })).toBe(true)
    expect(isNotMeasured({ d7: { clicks: 0, impressions: 0 }, d28: { clicks: 0, impressions: 0 }, top_queries_28d: [] })).toBe(false)
  })

  it('fetches channels and manages ingest keys at the P8 routes', async () => {
    const store = useGrowthStore()
    mockApi.get.mockResolvedValue({ keys: [] })
    mockApi.post.mockResolvedValue({ id: 'k1', key: 'rgi_x' })
    mockApi.delete.mockResolvedValue(undefined)
    await store.fetchChannels()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/channels')
    await store.fetchChannels({ since: '2026-10-01', until: '2026-10-13', includeInternal: true })
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/channels?since=2026-10-01&until=2026-10-13&include_internal=true',
    )
    await store.fetchKeys()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/keys')
    const minted = await store.mintKey('promotion pipeline')
    expect(mockApi.post).toHaveBeenCalledWith('/admin/growth/keys', {
      kind: 'ingest',
      label: 'promotion pipeline',
    })
    expect(minted.key).toBe('rgi_x')
    // P10 — the read kind is a second mint, never the default.
    await store.mintKey('pipeline results', 'read')
    expect(mockApi.post).toHaveBeenLastCalledWith('/admin/growth/keys', {
      kind: 'read',
      label: 'pipeline results',
    })
    await store.revokeKey('69a1dbbad2000f26adc875ce')
    expect(mockApi.delete).toHaveBeenCalledWith('/admin/growth/keys/69a1dbbad2000f26adc875ce')
  })

  it('reads the weekly roll-up and the alerts at the P10 routes', async () => {
    const store = useGrowthStore()
    mockApi.get.mockResolvedValue({ weeks: [], open: [], recent: [], rules: [] })
    mockApi.post.mockResolvedValue({ week: '2026-W41', status: 'preview' })
    await store.fetchWeekly()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/weekly')
    await store.fetchWeekly('2026-W41')
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/weekly?week=2026-W41')
    await store.fetchWeeklies('2026-W30', 10)
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/weeklies?since=2026-W30&limit=10')
    await store.runWeekly('2026-W41')
    expect(mockApi.post).toHaveBeenCalledWith('/admin/growth/weekly/run?week=2026-W41&send=false')
    await store.fetchAlerts()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/alerts')
    await store.fetchAlerts('2026-10-01')
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/alerts?since=2026-10-01')
    await store.evaluateAlerts()
    expect(mockApi.post).toHaveBeenLastCalledWith('/admin/growth/alerts/evaluate')
    await store.evaluateAlerts(true)
    expect(mockApi.post).toHaveBeenLastCalledWith('/admin/growth/alerts/evaluate?dry_run=true')
    // A missing week is null, not an error.
    mockApi.get.mockRejectedValueOnce(Object.assign(new Error('nf'), { status: 404 }))
    expect(await store.fetchWeekly('2020-W01')).toBeNull()
  })

  it('fetches the newsletter view at the P5 route with its range', async () => {
    const store = useGrowthStore()
    mockApi.get.mockResolvedValue({ issues: [] })
    await store.fetchNewsletter()
    expect(mockApi.get).toHaveBeenCalledWith('/admin/growth/newsletter')
    await store.fetchNewsletter({ since: '2026-07-17', until: '2026-10-14', includeInternal: true })
    expect(mockApi.get).toHaveBeenCalledWith(
      '/admin/growth/newsletter?since=2026-07-17&until=2026-10-14&include_internal=true',
    )
  })

  it('reads an unmeasured delivery as such, and a measured zero as a number', () => {
    // The two shapes the per-issue table's delivery cell takes.
    expect(isNotMeasured({ not_measured: 'the SendGrid Event Webhook is not verified' })).toBe(true)
    expect(
      isNotMeasured({ delivered: 0, bounced: 0, hard_bounced: 0, dropped: 0, complaints: 0 }),
    ).toBe(false)
  })
})
