// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-129 (#3022) A2 — the pure parts of the `/pair` page: the user code's
 * shape, the pending-pairing marker that brings a person back to `/pair`
 * across tabs, and the time and country helpers the device card needs.
 *
 * The page itself is `views/pair/PairView.vue`; the server side is A1.
 */

/** The 20 unambiguous consonants a user code is made of (the spec's §A.1). */
export const USER_CODE_ALPHABET = 'BCDFGHJKLMNPQRSTVWXZ'
/** Eight letters, shown as `XXXX-XXXX`. */
export const USER_CODE_LENGTH = 8

/**
 * What a person typed, reduced to what can be part of a code: upper-cased,
 * only the alphabet's letters (so a hyphen, a space, a vowel or a digit
 * typed by mistake vanishes), and at most eight of them.
 */
export function normalizeUserCode(raw: string): string {
  let out = ''
  for (const ch of raw.toUpperCase()) {
    if (!USER_CODE_ALPHABET.includes(ch)) continue
    out += ch
    if (out.length === USER_CODE_LENGTH) break
  }
  return out
}

/** `BCDFGHJK` → `BCDF-GHJK`; a partial code gets its hyphen once it passes four letters. */
export function formatUserCode(normalized: string): string {
  return normalized.length > 4 ? `${normalized.slice(0, 4)}-${normalized.slice(4)}` : normalized
}

export function isCompleteUserCode(normalized: string): boolean {
  return normalized.length === USER_CODE_LENGTH
}

// ── The pending-pairing marker ──────────────────────────────────────────────
//
// A person sent from the signed-out `/pair` to sign in or sign up must come
// back to `/pair`. In the same tab the router's `pending_redirect`
// (sessionStorage) does that. An email sign-up breaks the tab: the activation
// link opens a NEW tab from the mail client, where sessionStorage is empty. So
// the page also leaves this marker in localStorage, which every tab of the
// origin shares, and each "just signed in — where to?" decision (the router
// guard's signed-in branch, the sign-in page, the OAuth callback, the
// sign-up page) honours it while it is unexpired.
//
// ⚠️ The destination is the fixed literal `PAIR_PATH`, never a stored URL, so
// this adds no open-redirect surface: the marker says WHEN, not WHERE.

/** The page the marker leads to — a literal, never read from storage. */
export const PAIR_PATH = '/pair'
export const PENDING_PAIR_KEY = 'pending_pair_until'
/** A pairing lives 15 minutes (decision 2); the marker lives as long. */
export const PENDING_PAIR_TTL_MS = 15 * 60 * 1000

export type StorageLike = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>

/** `localStorage`, or null where even touching it throws (a locked-down private window). */
function defaultStorage(): StorageLike | null {
  try {
    return globalThis.localStorage ?? null
  } catch {
    return null
  }
}

/** Leave the marker: a pairing is waiting for this browser until `now + TTL`. */
export function rememberPendingPair(now = Date.now(), store: StorageLike | null = defaultStorage()): void {
  try {
    store?.setItem(PENDING_PAIR_KEY, String(now + PENDING_PAIR_TTL_MS))
  } catch {
    // Private mode, storage disabled: the same-tab return still works
    // through `pending_redirect`; only the cross-tab one is lost.
  }
}

export function clearPendingPair(store: StorageLike | null = defaultStorage()): void {
  try {
    store?.removeItem(PENDING_PAIR_KEY)
  } catch {
    /* nothing to clear where nothing could be written */
  }
}

/**
 * Is a pairing waiting? An expired or unreadable marker is cleared and
 * reads false, so a marker never outlives the pairing it stands for.
 */
export function hasPendingPair(now = Date.now(), store: StorageLike | null = defaultStorage()): boolean {
  let raw: string | null
  try {
    raw = store?.getItem(PENDING_PAIR_KEY) ?? null
  } catch {
    return false
  }
  if (raw === null) return false
  const until = Number(raw)
  if (Number.isFinite(until) && until > now) return true
  clearPendingPair(store)
  return false
}

/** Consume the marker: true once, and it is gone. */
export function takePendingPair(now = Date.now(), store: StorageLike | null = defaultStorage()): boolean {
  if (!hasPendingPair(now, store)) return false
  clearPendingPair(store)
  return true
}

// ── The device card's helpers ───────────────────────────────────────────────

export type ElapsedUnit = 'second' | 'minute' | 'hour' | 'day'
export interface Elapsed {
  unit: ElapsedUnit
  value: number
}

/**
 * How long ago `at` was, in the largest unit that fits; null for a value that
 * is not a time. A time in the future (a skewed clock) reads as now.
 */
export function elapsedSince(at: string | number | Date | null | undefined, now = Date.now()): Elapsed | null {
  if (at === null || at === undefined || at === '') return null
  const t = at instanceof Date ? at.getTime() : new Date(at).getTime()
  if (!Number.isFinite(t)) return null
  const s = Math.max(0, Math.round((now - t) / 1000))
  if (s < 60) return { unit: 'second', value: s }
  const m = Math.round(s / 60)
  if (m < 60) return { unit: 'minute', value: m }
  const h = Math.round(m / 60)
  if (h < 24) return { unit: 'hour', value: h }
  return { unit: 'day', value: Math.round(h / 24) }
}

/**
 * "3 minutes ago", in the reader's language where the browser can say it;
 * anything under a minute is `justNow` (the page's own words).
 */
export function formatElapsed(e: Elapsed, locale: string, justNow: string): string {
  if (e.unit === 'second') return justNow
  try {
    return new Intl.RelativeTimeFormat(locale, { numeric: 'always' }).format(-e.value, e.unit)
  } catch {
    return `${e.value} ${e.unit}${e.value === 1 ? '' : 's'} ago`
  }
}

/**
 * The country's name for a two-letter code ("DE" → "Germany") where the
 * browser knows it; any other value as it came; null for none. The server
 * sends what its GeoIP lookup gave, which may be a code or already a name.
 */
export function countryName(country: string | null | undefined, locale: string): string | null {
  const raw = (country ?? '').trim()
  if (!raw) return null
  if (!/^[A-Za-z]{2}$/.test(raw)) return raw
  try {
    return new Intl.DisplayNames([locale], { type: 'region' }).of(raw.toUpperCase()) ?? raw
  } catch {
    return raw
  }
}

/** `x86_64` → `x86-64`, `aarch64` → `ARM64`; anything else as it came. */
export function archLabel(arch: string | null | undefined): string {
  const raw = (arch ?? '').trim()
  switch (raw.toLowerCase()) {
    case 'x86_64':
    case 'amd64':
      return 'x86-64'
    case 'aarch64':
    case 'arm64':
      return 'ARM64'
    default:
      return raw
  }
}
