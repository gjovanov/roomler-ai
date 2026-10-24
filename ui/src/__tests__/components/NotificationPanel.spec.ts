// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #2841 - the bell must not navigate a live viewer or a call away from its
// session. Before the fix, `handleClick` ran `router.push(notif.link)` in
// whatever tab the bell was in. Field 2026-10-24: a consent request clicked
// there moved a live remote viewer to the consent page, and the device hung
// that session up.
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import NotificationPanel from '@/components/layout/NotificationPanel.vue'
import type { Notification } from '@/stores/notification'

const { store } = vi.hoisted(() => ({
  store: {
    notifications: [] as Notification[],
    loading: false,
    unreadCount: 0,
    markRead: (() => {}) as (id: string) => void,
    markAllRead: () => {},
    fetchNotifications: () => {},
  },
}))

vi.mock('@/stores/notification', () => ({
  useNotificationStore: () => store,
}))
vi.mock('@/composables/usePush', () => ({
  pushState: async () => 'granted',
  enablePushNotifications: async () => 'granted',
}))

const vuetify = createVuetify({ components, directives })
const Page = { template: '<div />' }

beforeAll(() => {
  const g = globalThis as unknown as Record<string, unknown>
  if (!('ResizeObserver' in g)) {
    g.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    }
  }
})

let wrapper: VueWrapper | null = null
afterEach(() => {
  wrapper?.unmount()
  wrapper = null
  vi.restoreAllMocks()
})

const consentRequest: Notification = {
  id: 'n1',
  notification_type: 'system',
  title: 'Remote control request',
  body: 'Someone wants to control a device',
  link: '/consent/t1',
  is_read: false,
  created_at: new Date().toISOString(),
}

async function panelAt(path: string): Promise<{ router: Router; wrapper: VueWrapper }> {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/devices', component: Page },
      { path: '/agent/:agentId/remote', component: Page, meta: { liveSession: true } },
      { path: '/consent/:token', component: Page },
    ],
  })
  await router.push(path)
  store.notifications = [{ ...consentRequest }]
  store.loading = false
  store.unreadCount = 1
  store.markRead = vi.fn()
  wrapper = mount(NotificationPanel, { global: { plugins: [vuetify, router] } })
  await flushPromises()
  return { router, wrapper }
}

function clickTheRequest(w: VueWrapper) {
  const item = w.findAll('.v-list-item').find((i) => i.text().includes('Remote control request'))
  expect(item, 'the request row').toBeDefined()
  return item!.trigger('click')
}

describe('NotificationPanel: a click on a notification (#2841)', () => {
  it('opens a consent request in a new tab from a live viewer, and the viewer stays', async () => {
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const { router, wrapper: w } = await panelAt('/agent/a1/remote')

    await clickTheRequest(w)
    await flushPromises()

    expect(open).toHaveBeenCalledWith('/consent/t1', '_blank', 'noopener')
    expect(router.currentRoute.value.fullPath).toBe('/agent/a1/remote')
    expect(store.markRead).toHaveBeenCalledWith('n1')
    expect(w.emitted('close')).toHaveLength(1)
  })

  it('still navigates in place from a page with no session', async () => {
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const { router, wrapper: w } = await panelAt('/devices')

    await clickTheRequest(w)
    await flushPromises()

    expect(open).not.toHaveBeenCalled()
    expect(router.currentRoute.value.fullPath).toBe('/consent/t1')
    expect(w.emitted('close')).toHaveLength(1)
  })
})
