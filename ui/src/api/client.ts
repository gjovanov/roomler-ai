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

    if (
      (resp.status === 401 || resp.status === 403) &&
      !AUTH_PATHS.some((p) => path.startsWith(p))
    ) {
      localStorage.removeItem('access_token')
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
