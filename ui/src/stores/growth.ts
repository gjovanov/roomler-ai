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

  return { digests, error, fetchDigests, fetchDigest, runDigest, fetchPreview }
})
