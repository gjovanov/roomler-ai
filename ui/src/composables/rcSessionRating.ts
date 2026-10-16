// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-102 P9b — the post-session rating's rules (spec §3f, D10): after a
 * remote session of 30 s or more the viewer offers a thumbs up or down with
 * optional reason chips, at most once a day per person, dismissible, never
 * blocking, never stealing focus, no free text.
 *
 * Every rule here is a pure function with a test, and every one of them
 * **fails toward not prompting**: no server feature read as `true`, no
 * session id, no request time, a session under 30 s, a day already prompted,
 * or a browser whose storage cannot record the day (so "once a day" could
 * not be kept) — each is a silent no. The prompt is a courtesy; a session is
 * not.
 *
 * ⚠️ The 30 s are measured from the viewer's **request** (the `connect()`
 * that set phase `requesting`) to the attempt's terminal transition
 * (`closed` or `error`) — a reconnect inside the attempt keeps the original
 * start. A connect that stalled for 35 s and was given up is exactly a
 * session worth asking about ("couldn't connect" is a chip); the server
 * refuses a rating posted sooner than 30 s after the request too.
 *
 * ⚠️ "Once a day" is the UTC day of the **prompt**, recorded when it is
 * shown — a dismissed prompt counts, so nobody is asked twice because they
 * said no. It is kept in `localStorage`, per browser: the same person on a
 * second browser may be asked there as well, and the server keeps one rating
 * per session regardless.
 */
import type { RcPhase } from './useRemoteControl'

export const RATING_MIN_SESSION_MS = 30_000
export const RATING_STORAGE_KEY = 'rc.rating.lastPromptDay'
/** The reason chips — the whole vocabulary; the server refuses any other. */
export const RATING_REASONS = ['lag', 'picture', 'input', 'could_not_connect', 'other'] as const
export type RatingReason = (typeof RATING_REASONS)[number]
export type Rating = 'up' | 'down'

/** `YYYY-MM-DD` of an instant, UTC. */
export function utcDay(nowMs: number): string {
  return new Date(nowMs).toISOString().slice(0, 10)
}

/** The two calls the rules make of a storage; `localStorage` qualifies. */
export interface RatingStorage {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
}

/** The window's `localStorage`, or `null` where touching it throws (a private
 *  window, blocked site data) — in which case "once a day" cannot be kept
 *  and the prompt is not offered. */
export function storageOrNull(): RatingStorage | null {
  try {
    const s = globalThis.localStorage
    if (!s) return null
    // A storage that throws on access is one that cannot keep the day.
    s.getItem(RATING_STORAGE_KEY)
    return s
  } catch {
    return null
  }
}

/** The UTC day the prompt was last shown, or `null`; never throws. */
export function readLastPromptDay(storage: RatingStorage | null | undefined): string | null {
  if (!storage) return null
  try {
    const v = storage.getItem(RATING_STORAGE_KEY)
    return v && /^\d{4}-\d{2}-\d{2}$/.test(v) ? v : null
  } catch {
    return null
  }
}

/** Record the day the prompt is shown; `false` when it could not be kept. */
export function markPrompted(storage: RatingStorage | null | undefined, day: string): boolean {
  if (!storage) return false
  try {
    storage.setItem(RATING_STORAGE_KEY, day)
    return storage.getItem(RATING_STORAGE_KEY) === day
  } catch {
    return false
  }
}

export interface OfferInput {
  /** `/api/capabilities` → `features.session_feedback`, read as `true`. */
  enabled: boolean
  sessionId: string | null
  requestedAtMs: number | null
  endedAtMs: number
  lastPromptDay: string | null
  /** Whether the day of the prompt can be recorded. */
  storageUsable: boolean
}

/** The decision: every gate must say yes. */
export function shouldOfferRating(i: OfferInput): boolean {
  if (!i.enabled) return false
  if (!i.sessionId) return false
  if (i.requestedAtMs === null || !Number.isFinite(i.requestedAtMs)) return false
  if (i.endedAtMs - i.requestedAtMs < RATING_MIN_SESSION_MS) return false
  if (!i.storageUsable) return false
  if (i.lastPromptDay === utcDay(i.endedAtMs)) return false
  return true
}

/** One attempt that just reached a terminal phase. */
export interface EndedAttempt {
  requestedAtMs: number
  /** The last session id the attempt was given; `null` on a `requesting` stall. */
  sessionId: string | null
}

const STARTS_FROM: readonly RcPhase[] = ['idle', 'closed', 'error']

/**
 * A tracker fed by the viewer's phase transitions and session ids. It knows
 * when an attempt started (`requesting` from a resting phase) and reports
 * the attempt once, when it reaches `closed` or `error`; a reconnect in
 * between is the same attempt.
 */
export function createRatingTracker(now: () => number = () => Date.now()) {
  let requestedAt: number | null = null
  let sessionId: string | null = null
  return {
    /** The viewer's `sessionId` as it changes; nulls (a teardown) are kept out. */
    onSession(id: string | null | undefined): void {
      if (typeof id === 'string' && id) sessionId = id
    },
    /** A phase transition; the attempt when this one ended it. */
    onPhase(prev: RcPhase | undefined, next: RcPhase): EndedAttempt | null {
      if (next === 'requesting' && (prev === undefined || STARTS_FROM.includes(prev))) {
        requestedAt = now()
        sessionId = null
        return null
      }
      if ((next === 'closed' || next === 'error') && requestedAt !== null) {
        const ended = { requestedAtMs: requestedAt, sessionId }
        requestedAt = null
        sessionId = null
        return ended
      }
      return null
    },
    /** For tests and the console. */
    state(): { requestedAtMs: number | null; sessionId: string | null } {
      return { requestedAtMs: requestedAt, sessionId }
    },
  }
}

export type RatingTracker = ReturnType<typeof createRatingTracker>

/** What the viewer posts; the server validates it again. */
export interface RatingBody {
  rating: Rating
  reasons: RatingReason[]
}

/** The route the viewer posts to, within the page's org. */
export function ratingPath(tenantId: string, sessionId: string): string {
  return `/tenant/${encodeURIComponent(tenantId)}/session/${encodeURIComponent(sessionId)}/feedback`
}
