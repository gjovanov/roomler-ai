// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-102 P9b — the post-session rating's rules: every gate fails toward not
// prompting; 30 s from the request to the end, a reconnect inside the
// attempt being the same attempt; once per UTC day, counted when shown; a
// storage that cannot keep the day means no prompt.
import { describe, it, expect } from 'vitest'
import {
  RATING_MIN_SESSION_MS,
  RATING_REASONS,
  RATING_STORAGE_KEY,
  createRatingTracker,
  markPrompted,
  ratingPath,
  readLastPromptDay,
  shouldOfferRating,
  utcDay,
  type RatingStorage,
} from '@/composables/rcSessionRating'

function memoryStorage(initial: Record<string, string> = {}): RatingStorage & { data: Record<string, string> } {
  const data = { ...initial }
  return {
    data,
    getItem: (k) => (k in data ? data[k] : null),
    setItem: (k, v) => {
      data[k] = v
    },
  }
}

const throwingStorage: RatingStorage = {
  getItem: () => {
    throw new Error('SecurityError')
  },
  setItem: () => {
    throw new Error('QuotaExceededError')
  },
}

const T0 = Date.UTC(2026, 9, 16, 10, 0, 0)

describe('shouldOfferRating', () => {
  const ok = {
    enabled: true,
    sessionId: '0123456789abcdef01234567',
    requestedAtMs: T0,
    endedAtMs: T0 + RATING_MIN_SESSION_MS,
    lastPromptDay: null,
    storageUsable: true,
  }

  it('offers only when every gate says yes', () => {
    expect(shouldOfferRating(ok)).toBe(true)
    expect(shouldOfferRating({ ...ok, enabled: false })).toBe(false)
    expect(shouldOfferRating({ ...ok, sessionId: null })).toBe(false)
    expect(shouldOfferRating({ ...ok, requestedAtMs: null })).toBe(false)
    expect(shouldOfferRating({ ...ok, requestedAtMs: Number.NaN })).toBe(false)
    expect(shouldOfferRating({ ...ok, storageUsable: false })).toBe(false)
  })

  it('never after a session shorter than 30 s, measured from the request', () => {
    expect(shouldOfferRating({ ...ok, endedAtMs: T0 + RATING_MIN_SESSION_MS - 1 })).toBe(false)
    expect(shouldOfferRating({ ...ok, endedAtMs: T0 + 5_000 })).toBe(false)
    expect(shouldOfferRating({ ...ok, endedAtMs: T0 + 31_000 })).toBe(true)
  })

  it('at most once per UTC day', () => {
    expect(shouldOfferRating({ ...ok, lastPromptDay: utcDay(ok.endedAtMs) })).toBe(false)
    expect(shouldOfferRating({ ...ok, lastPromptDay: '2026-10-15' })).toBe(true)
    // The day is UTC: 23:59Z and 00:01Z the next day are two days.
    const late = Date.UTC(2026, 9, 16, 23, 59, 0)
    expect(utcDay(late)).toBe('2026-10-16')
    expect(utcDay(late + 2 * 60_000)).toBe('2026-10-17')
  })
})

describe('the prompt day in storage', () => {
  it('reads a day, ignores junk, and never throws', () => {
    expect(readLastPromptDay(memoryStorage({ [RATING_STORAGE_KEY]: '2026-10-16' }))).toBe('2026-10-16')
    expect(readLastPromptDay(memoryStorage({ [RATING_STORAGE_KEY]: 'yesterday' }))).toBeNull()
    expect(readLastPromptDay(memoryStorage())).toBeNull()
    expect(readLastPromptDay(null)).toBeNull()
    expect(readLastPromptDay(throwingStorage)).toBeNull()
  })

  it('marks the day when shown, and says when it could not', () => {
    const s = memoryStorage()
    expect(markPrompted(s, '2026-10-16')).toBe(true)
    expect(s.data[RATING_STORAGE_KEY]).toBe('2026-10-16')
    expect(markPrompted(null, '2026-10-16')).toBe(false)
    expect(markPrompted(throwingStorage, '2026-10-16')).toBe(false)
  })
})

describe('createRatingTracker', () => {
  it('reports an attempt once, at its terminal phase, with its last session id', () => {
    let now = T0
    const t = createRatingTracker(() => now)
    expect(t.onPhase('idle', 'requesting')).toBeNull()
    t.onSession('aaaaaaaaaaaaaaaaaaaaaaaa')
    expect(t.onPhase('requesting', 'awaiting_consent')).toBeNull()
    expect(t.onPhase('awaiting_consent', 'negotiating')).toBeNull()
    expect(t.onPhase('negotiating', 'connected')).toBeNull()
    now = T0 + 45_000
    expect(t.onPhase('connected', 'closed')).toEqual({
      requestedAtMs: T0,
      sessionId: 'aaaaaaaaaaaaaaaaaaaaaaaa',
    })
    // Reported once: a second terminal transition has no attempt behind it.
    expect(t.onPhase('closed', 'closed')).toBeNull()
    expect(t.state()).toEqual({ requestedAtMs: null, sessionId: null })
  })

  it('keeps the original start across a reconnect, and the newest session id', () => {
    let now = T0
    const t = createRatingTracker(() => now)
    t.onPhase('closed', 'requesting')
    t.onSession('aaaaaaaaaaaaaaaaaaaaaaaa')
    t.onPhase('requesting', 'connected')
    now = T0 + 10_000
    expect(t.onPhase('connected', 'reconnecting')).toBeNull()
    // The teardown nulls the id; the re-created session brings a new one.
    t.onSession(null)
    t.onSession('bbbbbbbbbbbbbbbbbbbbbbbb')
    // A reconnect's `requesting` is not a new attempt.
    expect(t.onPhase('reconnecting', 'requesting')).toBeNull()
    t.onPhase('requesting', 'connected')
    now = T0 + 40_000
    expect(t.onPhase('connected', 'error')).toEqual({
      requestedAtMs: T0,
      sessionId: 'bbbbbbbbbbbbbbbbbbbbbbbb',
    })
  })

  it('a stall that never got a session id ends with none, and a fresh attempt resets', () => {
    const t = createRatingTracker(() => T0)
    expect(t.onPhase(undefined, 'requesting')).toBeNull()
    expect(t.onPhase('requesting', 'error')).toEqual({ requestedAtMs: T0, sessionId: null })
    t.onPhase('error', 'requesting')
    t.onSession('cccccccccccccccccccccccc')
    t.onPhase('requesting', 'requesting')
    expect(t.state().sessionId).toBe('cccccccccccccccccccccccc')
  })
})

describe('the wire', () => {
  it('names the route within the org, and the chips are the server vocabulary', () => {
    expect(ratingPath('t1', 's1')).toBe('/tenant/t1/session/s1/feedback')
    expect(ratingPath('a/b', 's')).toBe('/tenant/a%2Fb/session/s/feedback')
    expect([...RATING_REASONS]).toEqual(['lag', 'picture', 'input', 'could_not_connect', 'other'])
  })
})
