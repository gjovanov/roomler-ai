// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

import { api } from '@/api/client'
import {
  enablePushNotifications,
  pushState,
  shouldSubscribeOnLoad,
  subscribePush,
} from '@/composables/usePush'

const mockApi = vi.mocked(api)
// Not a key: only decoded, never verified, and deliberately nothing like one
// (a key-shaped literal trips the secret scanner on every commit).
const VAPID = 'test-not-a-key'

let permission: NotificationPermission
const requestPermission = vi.fn<() => Promise<NotificationPermission>>()
const subscribe = vi.fn()

beforeEach(() => {
  vi.clearAllMocks()
  permission = 'default'
  requestPermission.mockImplementation(async () => permission)
  subscribe.mockResolvedValue({
    toJSON: () => ({ endpoint: 'https://push.example/x', keys: { auth: 'a', p256dh: 'p' } }),
  })
  vi.stubGlobal('Notification', {
    get permission() {
      return permission
    },
    requestPermission,
  })
  vi.stubGlobal('PushManager', function PushManager() {})
  const registration = { pushManager: { subscribe } }
  Object.defineProperty(globalThis.navigator, 'serviceWorker', {
    configurable: true,
    value: { register: vi.fn(async () => registration), ready: Promise.resolve(registration) },
  })
  mockApi.get.mockResolvedValue({ vapid_public_key: VAPID })
  mockApi.post.mockResolvedValue({})
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('#2817 push is an opt-in', () => {
  it('a page load subscribes only where permission is already granted', () => {
    expect(shouldSubscribeOnLoad('granted')).toBe(true)
    for (const s of ['default', 'denied', 'unsupported', 'not_configured'] as const) {
      expect(shouldSubscribeOnLoad(s)).toBe(false)
    }
  })

  it('never prompts from a load or a sign-in', async () => {
    // The browser has not been asked yet: the load must not be the one to ask.
    await subscribePush()
    expect(requestPermission).not.toHaveBeenCalled()
    expect(subscribe).not.toHaveBeenCalled()
    expect(mockApi.post).not.toHaveBeenCalled()
  })

  it('re-subscribes silently where permission was already granted', async () => {
    permission = 'granted'
    await subscribePush()
    expect(requestPermission).not.toHaveBeenCalled()
    expect(subscribe).toHaveBeenCalledOnce()
    expect(mockApi.post).toHaveBeenCalledWith('/push/subscribe', {
      endpoint: 'https://push.example/x',
      keys: { auth: 'a', p256dh: 'p' },
    })
  })

  it('the opt-in asks the browser, and subscribes on a yes', async () => {
    requestPermission.mockImplementation(async () => {
      permission = 'granted'
      return permission
    })
    expect(await enablePushNotifications()).toBe('granted')
    expect(requestPermission).toHaveBeenCalledOnce()
    expect(subscribe).toHaveBeenCalledOnce()
  })

  it('a no stays a no: nothing is subscribed', async () => {
    requestPermission.mockImplementation(async () => {
      permission = 'denied'
      return permission
    })
    expect(await enablePushNotifications()).toBe('denied')
    expect(subscribe).not.toHaveBeenCalled()
  })

  it('reports a server without push, and never prompts for it', async () => {
    mockApi.get.mockResolvedValue({ vapid_public_key: '' })
    expect(await pushState()).toBe('not_configured')
    expect(await enablePushNotifications()).toBe('not_configured')
    expect(requestPermission).not.toHaveBeenCalled()
  })

  it("reports the browser's own permission when push is configured", async () => {
    expect(await pushState()).toBe('default')
    permission = 'denied'
    expect(await pushState()).toBe('denied')
  })
})
