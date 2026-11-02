// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3029 — every path the server builds into an email, a push or a redirect
// resolves to a real page of the SPA.
//
// For months the activation email linked `/auth/activate`, a path the router
// did not have, so every email signup landed on Not Found and could never
// sign in; the activation-success email linked `/auth/login`, which did not
// exist either. No build, type check or test could see it: the server and
// the SPA each held their half of the contract. This spec holds both halves
// in one place.
//
// ⚠️ When you add a link to an email or a push payload on the server, add its
// path here, in the shape the server sends it. The builders, as of this
// writing:
//   - crates/api/src/routes/auth.rs        activation_url → /auth/activate?userId=…&token=…
//                                          login_url      → /login (the success email)
//   - crates/api/src/routes/invite.rs      invite_url     → /invite/{code}
//   - crates/api/src/routes/oauth.rs       callback_redirect → /oauth/callback[#signup=1]
//   - crates/modules/fleet/src/consent_consumer.rs   the consent email and push → /consent/{token}
//   - crates/modules/saas/src/subscribe.rs the confirm email and the API's 303 → /newsletter/confirm/{token},
//                                          and the outcome pages → /newsletter/confirmed, /newsletter/unsubscribed
//   - crates/modules/saas/src/newsletter.rs unsubscribe_link → /api/subscribe/unsubscribe/{token}
//                                          (an API path that 303s to /newsletter/unsubscribed; not an SPA route)
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(async (path: string) =>
      path === '/capabilities'
        ? { version: 'test', modules: ['chat', 'conference', 'fleet', 'remote', 'network'] }
        : undefined,
    ),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
  ApiError: class extends Error {},
}))
// The pages themselves are not what is under test; keep the module graph small.
const Stub = { template: '<div />' }
vi.mock('@/views/auth/LoginView.vue', () => ({ default: Stub }))
vi.mock('@/views/auth/ActivateView.vue', () => ({ default: Stub }))
vi.mock('@/views/NotFoundView.vue', () => ({ default: Stub }))

import router from '@/plugins/router'

/** What the server sends, with a sample value where the path carries one. */
const EMAILED_PATHS: ReadonlyArray<{ path: string; from: string; signedOut: boolean }> = [
  { path: '/auth/activate?userId=bbbbbbbbbbbbbbbbbbbbbbbb&token=abc1234', from: 'auth.rs activation_url', signedOut: true },
  { path: '/login', from: 'auth.rs login_url (the activation-success email)', signedOut: true },
  { path: '/auth/login', from: 'auth.rs login_url before #3029 (emails already sent)', signedOut: true },
  { path: '/invite/abc123', from: 'invite.rs invite_url', signedOut: true },
  { path: '/consent/0123456789abcdef', from: 'consent_consumer.rs (email + push)', signedOut: true },
  { path: '/oauth/callback', from: 'oauth.rs callback_redirect', signedOut: true },
  { path: '/oauth/callback#signup=1', from: 'oauth.rs callback_redirect (a new account)', signedOut: true },
  { path: '/newsletter/confirm/0123456789abcdef', from: 'subscribe.rs (email + the 303)', signedOut: true },
  { path: '/newsletter/confirmed?status=ok', from: 'subscribe.rs (the outcome 303)', signedOut: true },
  { path: '/newsletter/unsubscribed', from: 'newsletter.rs (the unsubscribe 303)', signedOut: true },
]

beforeEach(() => {
  setActivePinia(createPinia())
  localStorage.clear()
  sessionStorage.clear()
})

afterEach(() => {
  localStorage.clear()
  sessionStorage.clear()
})

describe('every path the server emails or pushes resolves to a page', () => {
  for (const { path, from } of EMAILED_PATHS) {
    it(`${path}  (${from})`, () => {
      const r = router.resolve(path)
      expect(r.name, `${path} is Not Found`).not.toBe('not-found')
      expect(r.matched.length, `${path} matches nothing`).toBeGreaterThan(0)
    })
  }

  it('a path the server does NOT build is still Not Found (the check can fail)', () => {
    expect(router.resolve('/auth/no-such-page').name).toBe('not-found')
  })
})

describe('the links a signed-out person opens from an email need no session', () => {
  for (const { path, from, signedOut } of EMAILED_PATHS.filter((p) => p.signedOut)) {
    it(`${path}  (${from})`, () => {
      const r = router.resolve(path)
      expect(r.matched.some((m) => m.meta.auth), `${path} requires a session`).toBe(false)
    })
  }
})

describe('the activation link (#3029)', () => {
  it('lands on the activation page, reachable signed out AND signed in', async () => {
    localStorage.removeItem('roomler-signed-in')
    await router.push('/auth/activate?userId=bbbbbbbbbbbbbbbbbbbbbbbb&token=abc1234')
    expect(router.currentRoute.value.name).toBe('activate')
    expect(router.currentRoute.value.query).toEqual({ userId: 'bbbbbbbbbbbbbbbbbbbbbbbb', token: 'abc1234' })
    // No `meta.guest`: a signed-in browser opening another account's link must
    // not be bounced to the dashboard before the token is redeemed.
    localStorage.setItem('roomler-signed-in', '1')
    await router.push('/')
    await router.push('/auth/activate?userId=bbbbbbbbbbbbbbbbbbbbbbbb&token=abc1234')
    expect(router.currentRoute.value.name).toBe('activate')
  })

  it('the success email’s old path still reaches sign-in, query and all', async () => {
    localStorage.removeItem('roomler-signed-in')
    await router.push('/auth/login?invite=abc')
    expect(router.currentRoute.value.name).toBe('login')
    expect(router.currentRoute.value.path).toBe('/login')
    expect(router.currentRoute.value.query).toEqual({ invite: 'abc' })
  })
})
