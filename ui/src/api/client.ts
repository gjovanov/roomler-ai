import router from '@/plugins/router'
import { useSnackbar } from '@/composables/useSnackbar'

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
    super(`API error ${status}`)
  }
}

function getToken(): string | null {
  return localStorage.getItem('access_token')
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
  const refreshToken = localStorage.getItem('refresh_token')
  if (!refreshToken) return 'failed'
  try {
    const resp = await fetch(`${BASE_URL}/auth/refresh`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ refresh_token: refreshToken }),
    })
    if (resp.status === 429) return 'throttled'
    if (!resp.ok) return 'failed'
    const data = await resp.json()
    if (data.access_token) {
      localStorage.setItem('access_token', data.access_token)
      if (data.refresh_token) {
        localStorage.setItem('refresh_token', data.refresh_token)
      }
      return 'ok'
    }
    return 'failed'
  } catch {
    return 'failed'
  }
}

async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const { method = 'GET', body, headers = {} } = options
  const token = getToken()

  const fetchHeaders: Record<string, string> = {
    ...headers,
  }

  if (token) {
    fetchHeaders['Authorization'] = `Bearer ${token}`
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
        // Retry the original request with the new token
        const retryHeaders = { ...fetchHeaders, Authorization: `Bearer ${getToken()}` }
        const retryResp = await fetch(`${BASE_URL}${path}`, {
          method,
          headers: retryHeaders,
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
        localStorage.removeItem('access_token')
        localStorage.removeItem('refresh_token')
        router.push({ name: 'login' })
      }
    }

    if (
      resp.status === 403 &&
      !AUTH_PATHS.some((p) => path.startsWith(p))
    ) {
      localStorage.removeItem('access_token')
      localStorage.removeItem('refresh_token')
      router.push({ name: 'login' })
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
