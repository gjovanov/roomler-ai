// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-88 (#1790) §3b — conversion goals, counted by purestat.
 *
 * purestat's script — injected by `analytics.ts` in the SPA and by the shell's
 * loader on every static page, on the hosted site only (FR-102, #2127) —
 * defines `window.purestat(name)`, and each goal below has a goal of the
 * same name in purestat, which attributes it to the visit's source. The
 * static pages fire the same names from `docs.js` and `home.js`.
 *
 * A call is a NO-OP when the script is absent (blocked, not loaded yet,
 * turned off, or not this host: a self-hosted deployment never loads it) and
 * it never throws: analytics must not be able to break a sign-up, a
 * subscription or a copy.
 *
 * ⚠️ purestat sends `location.href` with every event. Fire a goal only once
 * the URL holds nothing private — see `OAuthCallbackView.vue`, whose URL
 * arrives with the access token in its fragment.
 */

export const GOALS = ['signup', 'subscribe', 'install-copy', 'github-outbound'] as const
export type Goal = (typeof GOALS)[number]

type Purestat = (name: string, opts?: { props?: Record<string, string> }) => void

export function trackGoal(goal: Goal): void {
  try {
    const purestat = (window as Window & { purestat?: Purestat }).purestat
    if (typeof purestat === 'function') purestat(goal)
  } catch {
    /* analytics must never break the flow it measures */
  }
}

/**
 * Did the OAuth callback that landed here CREATE the account? The server says
 * so with `signup=1`, in the fragment (preferred: a fragment never reaches a
 * server log) or the query. Without the marker this is `false`, so a sign-in
 * to an existing account is never counted as a sign-up.
 */
export function isOAuthSignup(hash: string, search: string): boolean {
  const marked = (s: string) => new URLSearchParams(s.replace(/^[#?]/, '')).get('signup') === '1'
  return marked(hash) || marked(search)
}
