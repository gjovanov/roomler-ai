// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #3029 — the account activation an email link performs.
 *
 * `POST /api/auth/register` emails `{frontend_url}/auth/activate?userId=…&token=…`
 * (`crates/api/src/routes/auth.rs`). The page at that path
 * (`views/auth/ActivateView.vue`) redeems the token with
 * `POST /api/auth/activate {user_id, token}` (the server's `ActivateRequest`
 * has no `rename_all`, so the keys are snake_case). Until the page existed the
 * link rendered Not Found, and no email signup could ever sign in.
 *
 * The pure parts live here so they can be tested without a mount: reading
 * the two parameters, the one-POST-per-link guards, and the request itself.
 */

export interface ActivationParams {
  userId: string
  token: string
}

/** A router `LocationQuery`: each value a string, an array of them, or null. */
export type QueryLike = Readonly<Record<string, string | null | undefined | ReadonlyArray<string | null>>>

function first(query: QueryLike, key: string): string {
  const raw = query[key]
  const v = Array.isArray(raw) ? raw[0] : raw
  return typeof v === 'string' ? v.trim() : ''
}

/** Both parameters, or null when either is missing: no request is made for a half link. */
export function activationParams(query: QueryLike): ActivationParams | null {
  const userId = first(query, 'userId')
  const token = first(query, 'token')
  return userId && token ? { userId, token } : null
}

export type ActivationOutcome = 'ok' | 'invalid' | 'error'

// ── One POST per link ───────────────────────────────────────────────────────
//
// The token is single-use: the server deletes it on success, so a second
// POST — a refresh, a second mount of the page, a double-fired effect —
// answers 400 and would show "invalid or expired" to a person whose account
// IS active. Two guards: in-flight requests are shared per link (a second
// caller joins the first), and a success is remembered for the tab
// (sessionStorage), so a refresh shows the success again without asking.

const DONE_PREFIX = 'account_activated:'

export type StorageLike = Pick<Storage, 'getItem' | 'setItem'>

function defaultStorage(): StorageLike | null {
  try {
    return globalThis.sessionStorage ?? null
  } catch {
    return null
  }
}

export function wasActivated(userId: string, store: StorageLike | null = defaultStorage()): boolean {
  try {
    return store?.getItem(DONE_PREFIX + userId) === '1'
  } catch {
    return false
  }
}

export function rememberActivated(userId: string, store: StorageLike | null = defaultStorage()): void {
  try {
    store?.setItem(DONE_PREFIX + userId, '1')
  } catch {
    // Private mode: a refresh asks the server again and sees a 400; the
    // page's words for that case cover "if you already activated, sign in".
  }
}

const inflight = new Map<string, Promise<ActivationOutcome>>()

type FetchLike = (input: string, init?: RequestInit) => Promise<Pick<Response, 'ok' | 'status'>>

/**
 * Redeem the token. `ok` once the server accepted it (and remembered for the
 * tab); `invalid` on the server's 400 (unknown, expired or already-used
 * token); `error` for anything else — the server unreachable, a 5xx, a 429 —
 * where nothing is known and trying again is right.
 *
 * Concurrent calls for the same link share one request. A settled request is
 * forgotten, so "Try again" after an error really asks again.
 */
export function activateAccount(
  params: ActivationParams,
  fetchImpl: FetchLike = (input, init) => fetch(input, init),
): Promise<ActivationOutcome> {
  const key = `${params.userId}:${params.token}`
  const running = inflight.get(key)
  if (running) return running
  const p = (async (): Promise<ActivationOutcome> => {
    try {
      const r = await fetchImpl('/api/auth/activate', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ user_id: params.userId, token: params.token }),
      })
      if (r.ok) {
        rememberActivated(params.userId)
        return 'ok'
      }
      return r.status === 400 ? 'invalid' : 'error'
    } catch {
      return 'error'
    } finally {
      inflight.delete(key)
    }
  })()
  inflight.set(key, p)
  return p
}

export type ResendOutcome = 'sent' | 'throttled' | 'error'

/**
 * #3199 — ask for a new link. The server answers the same 202 for every
 * address and does the work afterwards, so `sent` means only that the request
 * was taken, and the page must not claim more. `throttled` is the per-client
 * 429; `error` is anything else, where trying again is right.
 */
export async function requestActivationLink(
  email: string,
  fetchImpl: FetchLike = (input, init) => fetch(input, init),
): Promise<ResendOutcome> {
  try {
    const r = await fetchImpl('/api/auth/resend-activation', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ email }),
    })
    if (r.ok) return 'sent'
    return r.status === 429 ? 'throttled' : 'error'
  } catch {
    return 'error'
  }
}
