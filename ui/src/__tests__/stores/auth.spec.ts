import { describe, it, expect, vi, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

// Mock router
vi.mock('@/plugins/router', () => ({
  default: { push: vi.fn() },
}))

// Mock push composable
vi.mock('@/composables/usePush', () => ({
  subscribePush: vi.fn(() => Promise.resolve()),
  unsubscribePush: vi.fn(() => Promise.resolve()),
}))

// Mock API client
vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

import { useAuthStore } from '@/stores/auth'
import { api } from '@/api/client'
import router from '@/plugins/router'
import { subscribePush, unsubscribePush } from '@/composables/usePush'

const mockApi = vi.mocked(api)
const mockRouter = vi.mocked(router)

describe('useAuthStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    localStorage.clear()
    vi.clearAllMocks()
  })

  describe('initial state', () => {
    it('should start with no user and not authenticated', () => {
      const store = useAuthStore()
      expect(store.user).toBeNull()
      expect(store.isAuthenticated).toBe(false)
      expect(store.loading).toBe(false)
      expect(store.error).toBeNull()
    })

    it('should pick up token from localStorage on creation', () => {
      localStorage.setItem('access_token', 'existing-token')
      const store = useAuthStore()
      expect(store.token).toBe('existing-token')
      expect(store.isAuthenticated).toBe(true)
    })
  })

  describe('login', () => {
    it('should store tokens in localStorage and set user', async () => {
      const mockUser = { id: '1', email: 'test@test.com', username: 'testuser', display_name: 'Test' }
      mockApi.post.mockResolvedValueOnce({ access_token: 'new-token', user: mockUser })

      const store = useAuthStore()
      await store.login('testuser', 'password123')

      expect(mockApi.post).toHaveBeenCalledWith('/auth/login', {
        username: 'testuser',
        password: 'password123',
      })
      expect(store.token).toBe('new-token')
      expect(store.user).toEqual(mockUser)
      expect(localStorage.getItem('access_token')).toBe('new-token')
      expect(store.isAuthenticated).toBe(true)
      expect(subscribePush).toHaveBeenCalled()
    })

    it('should set loading to true during login and false after', async () => {
      mockApi.post.mockResolvedValueOnce({ access_token: 'tok', user: { id: '1', email: '', username: '', display_name: '' } })

      const store = useAuthStore()
      const promise = store.login('u', 'p')
      // loading is set synchronously before the await
      expect(store.loading).toBe(true)
      await promise
      expect(store.loading).toBe(false)
    })

    it('should set error and rethrow on login failure', async () => {
      mockApi.post.mockRejectedValueOnce(new Error('Invalid credentials'))

      const store = useAuthStore()
      await expect(store.login('u', 'p')).rejects.toThrow('Invalid credentials')
      expect(store.error).toBe('Invalid credentials')
      expect(store.loading).toBe(false)
      expect(store.user).toBeNull()
    })
  })

  describe('logout', () => {
    it('should clear tokens, user, localStorage and redirect to login', async () => {
      // Set up authenticated state
      mockApi.post.mockResolvedValueOnce({
        access_token: 'tok',
        user: { id: '1', email: 'a@b.c', username: 'u', display_name: 'U' },
      })
      const store = useAuthStore()
      await store.login('u', 'p')

      store.logout()

      expect(store.token).toBeNull()
      expect(store.user).toBeNull()
      expect(localStorage.getItem('access_token')).toBeNull()
      expect(store.isAuthenticated).toBe(false)
      expect(unsubscribePush).toHaveBeenCalled()
      expect(mockRouter.push).toHaveBeenCalledWith({ name: 'login' })
    })
  })

  describe('isAuthenticated', () => {
    it('should return true when token is set', () => {
      localStorage.setItem('access_token', 'some-token')
      const store = useAuthStore()
      expect(store.isAuthenticated).toBe(true)
    })

    it('should return false when token is null', () => {
      const store = useAuthStore()
      expect(store.isAuthenticated).toBe(false)
    })
  })

  describe('fetchMe', () => {
    it('should fetch user from API and set user', async () => {
      localStorage.setItem('access_token', 'tok')
      const mockUser = { id: '2', email: 'me@test.com', username: 'me', display_name: 'Me' }
      mockApi.get.mockResolvedValueOnce(mockUser)

      const store = useAuthStore()
      await store.fetchMe()

      expect(mockApi.get).toHaveBeenCalledWith('/auth/me')
      expect(store.user).toEqual(mockUser)
      expect(subscribePush).toHaveBeenCalled()
    })

    it('should not call API if no token exists', async () => {
      const store = useAuthStore()
      await store.fetchMe()
      expect(mockApi.get).not.toHaveBeenCalled()
    })

    it('should call logout on fetchMe failure', async () => {
      localStorage.setItem('access_token', 'expired-tok')
      mockApi.get.mockRejectedValueOnce(new Error('Unauthorized'))

      const store = useAuthStore()
      await store.fetchMe()

      expect(store.token).toBeNull()
      expect(store.user).toBeNull()
      expect(mockRouter.push).toHaveBeenCalledWith({ name: 'login' })
    })
  })

  describe('register', () => {
    it('should register user, store token, and set user', async () => {
      const mockUser = { id: '3', email: 'new@test.com', username: 'newuser', display_name: 'New' }
      mockApi.post.mockResolvedValueOnce({ access_token: 'reg-token', user: mockUser })

      const store = useAuthStore()
      const result = await store.register('new@test.com', 'newuser', 'pass', 'New')

      expect(mockApi.post).toHaveBeenCalledWith('/auth/register', {
        email: 'new@test.com',
        username: 'newuser',
        password: 'pass',
        display_name: 'New',
      })
      expect(store.token).toBe('reg-token')
      expect(store.user).toEqual(mockUser)
      expect(localStorage.getItem('access_token')).toBe('reg-token')
      expect(result).toEqual({ access_token: 'reg-token', user: mockUser })
    })

    it('should include invite_code when provided', async () => {
      mockApi.post.mockResolvedValueOnce({
        access_token: 'tok',
        user: { id: '1', email: '', username: '', display_name: '' },
      })

      const store = useAuthStore()
      await store.register('e@e.com', 'u', 'p', 'D', 'invite-123')

      expect(mockApi.post).toHaveBeenCalledWith('/auth/register', {
        email: 'e@e.com',
        username: 'u',
        password: 'p',
        display_name: 'D',
        invite_code: 'invite-123',
      })
    })
  })
})
