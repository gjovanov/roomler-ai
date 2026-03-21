import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'

// Mock router
vi.mock('@/plugins/router', () => ({
  default: { push: vi.fn() },
}))

// Mock snackbar
const mockShowError = vi.fn()
vi.mock('@/composables/useSnackbar', () => ({
  useSnackbar: () => ({ showError: mockShowError }),
}))

import { api, ApiError } from '@/api/client'
import router from '@/plugins/router'

const mockRouter = vi.mocked(router)

describe('api client', () => {
  let mockFetch: ReturnType<typeof vi.fn>

  beforeEach(() => {
    localStorage.clear()
    vi.clearAllMocks()

    mockFetch = vi.fn()
    globalThis.fetch = mockFetch as typeof globalThis.fetch
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  function mockJsonResponse(status: number, data: unknown, ok?: boolean) {
    return {
      ok: ok ?? (status >= 200 && status < 300),
      status,
      headers: { get: (name: string) => name === 'content-type' ? 'application/json' : null },
      json: () => Promise.resolve(data),
      blob: () => Promise.resolve(new Blob()),
    }
  }

  describe('auth token injection', () => {
    it('should include Authorization header when token exists', async () => {
      localStorage.setItem('access_token', 'my-token')
      mockFetch.mockResolvedValueOnce(mockJsonResponse(200, { ok: true }))

      await api.get('/test')

      expect(mockFetch).toHaveBeenCalledWith('/api/test', expect.objectContaining({
        headers: expect.objectContaining({
          Authorization: 'Bearer my-token',
        }),
      }))
    })

    it('should not include Authorization header when no token', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(200, { ok: true }))

      await api.get('/test')

      const headers = mockFetch.mock.calls[0][1].headers
      expect(headers.Authorization).toBeUndefined()
    })
  })

  describe('request methods', () => {
    beforeEach(() => {
      mockFetch.mockResolvedValue(mockJsonResponse(200, { data: 'test' }))
    })

    it('api.get should use GET method', async () => {
      await api.get('/resource')
      expect(mockFetch).toHaveBeenCalledWith('/api/resource', expect.objectContaining({ method: 'GET' }))
    })

    it('api.post should use POST method and send JSON body', async () => {
      await api.post('/resource', { name: 'test' })
      expect(mockFetch).toHaveBeenCalledWith('/api/resource', expect.objectContaining({
        method: 'POST',
        body: JSON.stringify({ name: 'test' }),
        headers: expect.objectContaining({ 'Content-Type': 'application/json' }),
      }))
    })

    it('api.put should use PUT method', async () => {
      await api.put('/resource', { name: 'updated' })
      expect(mockFetch).toHaveBeenCalledWith('/api/resource', expect.objectContaining({
        method: 'PUT',
        body: JSON.stringify({ name: 'updated' }),
      }))
    })

    it('api.delete should use DELETE method', async () => {
      await api.delete('/resource')
      expect(mockFetch).toHaveBeenCalledWith('/api/resource', expect.objectContaining({ method: 'DELETE' }))
    })

    it('api.upload should send FormData without Content-Type header', async () => {
      const form = new FormData()
      form.append('file', new Blob(), 'test.txt')

      await api.upload('/upload', form)

      const callArgs = mockFetch.mock.calls[0][1]
      expect(callArgs.method).toBe('POST')
      expect(callArgs.body).toBe(form)
      expect(callArgs.headers['Content-Type']).toBeUndefined()
    })
  })

  describe('response handling', () => {
    it('should parse JSON responses', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(200, { name: 'test' }))
      const result = await api.get<{ name: string }>('/test')
      expect(result).toEqual({ name: 'test' })
    })

    it('should return blob for non-JSON responses', async () => {
      const mockBlob = new Blob(['data'])
      mockFetch.mockResolvedValueOnce({
        ok: true,
        status: 200,
        headers: { get: () => 'application/octet-stream' },
        blob: () => Promise.resolve(mockBlob),
      })

      const result = await api.get('/file')
      expect(result).toBe(mockBlob)
    })
  })

  describe('error handling', () => {
    it('should throw ApiError on non-ok response', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(400, { error: 'Bad request' }, false))

      await expect(api.get('/test')).rejects.toThrow(ApiError)
    })

    it('should include status and data on ApiError', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(422, { error: 'Validation failed' }, false))

      try {
        await api.get('/test')
        expect.unreachable('Should have thrown')
      } catch (err) {
        expect(err).toBeInstanceOf(ApiError)
        expect((err as ApiError).status).toBe(422)
        expect((err as ApiError).data).toEqual({ error: 'Validation failed' })
      }
    })

    it('should redirect to login on 401 for non-auth paths', async () => {
      localStorage.setItem('access_token', 'old-token')
      mockFetch.mockResolvedValueOnce(mockJsonResponse(401, {}, false))

      await expect(api.get('/tenant/123/room')).rejects.toThrow()

      expect(localStorage.getItem('access_token')).toBeNull()
      expect(mockRouter.push).toHaveBeenCalledWith({ name: 'login' })
    })

    it('should redirect to login on 403 for non-auth paths', async () => {
      localStorage.setItem('access_token', 'old-token')
      mockFetch.mockResolvedValueOnce(mockJsonResponse(403, {}, false))

      await expect(api.get('/tenant/123/room')).rejects.toThrow()

      expect(localStorage.getItem('access_token')).toBeNull()
      expect(mockRouter.push).toHaveBeenCalledWith({ name: 'login' })
    })

    it('should NOT redirect on 401 for auth paths', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(401, { error: 'Invalid credentials' }, false))

      await expect(api.post('/auth/login', {})).rejects.toThrow()

      expect(mockRouter.push).not.toHaveBeenCalled()
    })

    it('should NOT redirect on 401 for refresh path', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(401, {}, false))

      await expect(api.post('/auth/refresh', {})).rejects.toThrow()

      expect(mockRouter.push).not.toHaveBeenCalled()
    })

    it('should NOT redirect on 401 for oauth paths', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(401, {}, false))

      await expect(api.get('/oauth/google')).rejects.toThrow()

      expect(mockRouter.push).not.toHaveBeenCalled()
    })

    it('should show snackbar on 500 errors', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(500, { error: 'Internal server error' }, false))

      await expect(api.get('/test')).rejects.toThrow()

      expect(mockShowError).toHaveBeenCalledWith('Internal server error')
    })

    it('should show generic message when 500 response has no error field', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(500, {}, false))

      await expect(api.get('/test')).rejects.toThrow()

      expect(mockShowError).toHaveBeenCalledWith('Server error (500)')
    })

    it('should show message field if error field is missing on 500', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(502, { message: 'Bad gateway' }, false))

      await expect(api.get('/test')).rejects.toThrow()

      expect(mockShowError).toHaveBeenCalledWith('Bad gateway')
    })

    it('should not show snackbar for 4xx errors', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(400, { error: 'Bad request' }, false))

      await expect(api.get('/test')).rejects.toThrow()

      expect(mockShowError).not.toHaveBeenCalled()
    })
  })

  describe('base URL', () => {
    it('should prepend /api to all paths', async () => {
      mockFetch.mockResolvedValueOnce(mockJsonResponse(200, {}))

      await api.get('/tenant/123')

      expect(mockFetch).toHaveBeenCalledWith('/api/tenant/123', expect.anything())
    })
  })
})
