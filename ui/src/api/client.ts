// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import router from '@/plugins/router'
import { useSnackbar } from '@/composables/useSnackbar'
import { clearSignedIn } from '@/api/session'

const BASE_URL = '/api'

const AUTH_PATHS = ['/auth/login', '/auth/register', '/auth/refresh', '/oauth/']

interface RequestOptions {
  method?: string
  body?: unknown
  headers?: Record<string, string>
}

class ApiError extends Error {
  constructor(
    public status: number,
    public data: unknown,
  ) {
    // Prefer the server's explanation. Every ApiError variant serialises as
    // `{error, message}` (crates/api/src/error.rs), and call sites almost
    // universally render `(e as Error).message` straight into a form error —
    // so without this the user sees "API error 403" where the server took the
    // trouble to say WHICH permission it refused.
    super((data as Record<string, string> | null)?.message || `API error ${status}`)
  }
}

/**
 * End the session locally after the server has rejected it.
 *
 * Only clears the local hint — the cookie is `HttpOnly`, so the SERVER has to
 * expire it, which is what `/auth/logout` is for. This path is the involuntary
 * one (the credential was already refused), so there is nothing left to revoke.
 */
function endSessionLocally(): void {
  clearSignedIn()
  router.push({ name: 'login' })
}

/**
 * Turn a 429 into something a user can act on. The server sends `Retry-After`
 * in seconds; without it we can only say "too many requests".
 */
export function rateLimitMessage(resp: Response, data: unknown): string {
  const serverMessage = (data as Record<string, string> | null)?.message
  if (serverMessage) return serverMessage

  const retryAfter = Number(resp.headers.get('retry-after'))
  return Number.isFinite(retryAfter) && retryAfter > 0
    ? `Too many requests. Try again in ${retryAfter}s.`
    : 'Too many requests. Please wait a moment and try again.'
}

/**
 * 'throttled' is deliberately distinct from 'failed': a rate-limited refresh
 * tells us nothing about whether the session is still valid, so it must not
 * trigger the logout that a genuine rejection does.
 */
export type RefreshOutcome = 'ok' | 'failed' | 'throttled'

let refreshPromise: Promise<RefreshOutcome> | null = null

async function tryRefreshToken(): Promise<RefreshOutcome> {
  // Deduplicate concurrent refresh attempts
  if (refreshPromise) return refreshPromise
  refreshPromise = doRefresh()
  const result = await refreshPromise
  refreshPromise = null
  return result
}

async function doRefresh(): Promise<RefreshOutcome> {
  // No token to send and none to keep: the refresh credential is an HttpOnly
  // cookie scoped to this exact endpoint, so the browser attaches it and the
  // response replaces it. An empty body is the whole request.
  try {
    const resp = await fetch(`${BASE_URL}/auth/refresh`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: '{}',
    })
    if (resp.status === 429) return 'throttled'
    if (!resp.ok) return 'failed'
    // The new access token arrives as a Set-Cookie; the body copy is ignored.
    return 'ok'
  } catch {
    return 'failed'
  }
}

async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const { method = 'GET', body, headers = {} } = options

  // No Authorization header. `BASE_URL` is `/api`, so every call here is
  // same-origin and the browser attaches the `access_token` cookie itself —
  // which the server has always accepted. Reading a token in JS to put it back
  // on the request bought nothing except a credential sitting in localStorage
  // for an XSS to find.
  const fetchHeaders: Record<string, string> = {
    ...headers,
  }

  if (body && !(body instanceof FormData)) {
    fetchHeaders['Content-Type'] = 'application/json'
  }

  const resp = await fetch(`${BASE_URL}${path}`, {
    method,
    headers: fetchHeaders,
    body: body instanceof FormData ? body : body ? JSON.stringify(body) : undefined,
  })

  if (!resp.ok) {
    const data = await resp.json().catch(() => ({}))

    if (resp.status === 429) {
      // Being throttled says nothing about the session, so never treat it
      // as an auth failure — the 401 branch below would log the user out
      // and bounce them to a login page that is itself throttled.
      const { showError } = useSnackbar()
      showError(rateLimitMessage(resp, data))
      throw new ApiError(resp.status, data)
    }

    if (
      resp.status === 401 &&
      !AUTH_PATHS.some((p) => path.startsWith(p))
    ) {
      // Try to refresh the token before giving up
      const outcome = await tryRefreshToken()
      if (outcome === 'ok') {
        // Retry. Nothing to re-attach: the refresh response replaced the
        // session cookie, so the retry carries the new one automatically.
        const retryResp = await fetch(`${BASE_URL}${path}`, {
          method,
          headers: fetchHeaders,
          body: body instanceof FormData ? body : body ? JSON.stringify(body) : undefined,
        })
        if (retryResp.ok) {
          const ct = retryResp.headers.get('content-type') || ''
          if (ct.includes('application/json')) return retryResp.json() as Promise<T>
          return retryResp.blob() as unknown as Promise<T>
        }
      }
      if (outcome === 'throttled') {
        // Keep the session: we never learned whether it was still valid.
        const { showError } = useSnackbar()
        showError('Too many requests. Please wait a moment and try again.')
      } else {
        // Refresh was genuinely rejected, or the retry failed — force logout
        endSessionLocally()
      }
    }

    if (
      resp.status === 403 &&
      method === 'GET' &&
      !AUTH_PATHS.some((p) => path.startsWith(p))
    ) {
      // 403 is an AUTHORIZATION verdict, not an authentication one — the
      // token was accepted and the server then made a policy decision. The
      // logout exists for the navigation case (membership revoked, tenant
      // switched under you ⇒ every GET 403s and the UI would sit there
      // broken), so it stays on GET only.
      //
      // On a MUTATION it is actively wrong: the role editor refusing to
      // grant a permission the caller doesn't hold is a normal, expected
      // answer that the dialog already knows how to display. Logging the
      // user out instead would make a deliberate refusal look like a crash.
      endSessionLocally()
    }

    if (resp.status >= 500) {
      const msg = (data as Record<string, string>)?.error || (data as Record<string, string>)?.message || `Server error (${resp.status})`
      const { showError } = useSnackbar()
      showError(msg)
    }

    throw new ApiError(resp.status, data)
  }

  const contentType = resp.headers.get('content-type') || ''
  if (contentType.includes('application/json')) {
    return resp.json() as Promise<T>
  }
  return resp.blob() as unknown as Promise<T>
}

export const api = {
  get: <T>(path: string) => request<T>(path),
  post: <T>(path: string, body?: unknown) => request<T>(path, { method: 'POST', body }),
  put: <T>(path: string, body?: unknown) => request<T>(path, { method: 'PUT', body }),
  delete: <T>(path: string) => request<T>(path, { method: 'DELETE' }),
  upload: <T>(path: string, formData: FormData) =>
    request<T>(path, { method: 'POST', body: formData }),
}

export { ApiError }
