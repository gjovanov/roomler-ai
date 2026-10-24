// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #2841 - a link clicked inside the app must never take a tab away from the
// live session it holds. Measured 2026-10-24: a consent request clicked in
// the bell moved a live remote viewer to the consent page, and the device
// hung that session up three seconds later. #2828 gave a push card's click
// the same rule.
import { afterEach, describe, expect, it, vi } from 'vitest'
import { flushPromises } from '@vue/test-utils'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import realRouter from '@/plugins/router'
import { holdsLiveSession, openInAppLink } from '@/utils/inAppLink'

const Page = { template: '<div />' }

async function routerAt(path: string): Promise<Router> {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/devices', component: Page },
      { path: '/agent/:agentId/remote', component: Page, meta: { liveSession: true } },
      { path: '/consent/:token', component: Page },
    ],
  })
  await router.push(path)
  return router
}

afterEach(() => {
  vi.restoreAllMocks()
})

describe('openInAppLink (#2841)', () => {
  it('opens the link in a new tab from a live session, and leaves this tab where it is', async () => {
    const router = await routerAt('/agent/a1/remote')
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const push = vi.spyOn(router, 'push')

    expect(openInAppLink(router, '/consent/t1')).toBe('new-tab')
    await flushPromises()

    expect(open).toHaveBeenCalledWith('/consent/t1', '_blank', 'noopener')
    expect(push).not.toHaveBeenCalled()
    expect(router.currentRoute.value.fullPath).toBe('/agent/a1/remote')
  })

  it('navigates in place from any other page, as the bell always did', async () => {
    const router = await routerAt('/devices')
    const open = vi.spyOn(window, 'open').mockReturnValue(null)

    expect(openInAppLink(router, '/consent/t1')).toBe('navigated')
    await flushPromises()

    expect(open).not.toHaveBeenCalled()
    expect(router.currentRoute.value.fullPath).toBe('/consent/t1')
  })

  it('does nothing for a link to the page already showing, live or not', async () => {
    const router = await routerAt('/agent/a1/remote')
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const push = vi.spyOn(router, 'push')

    expect(openInAppLink(router, '/agent/a1/remote')).toBe('here')
    expect(open).not.toHaveBeenCalled()
    expect(push).not.toHaveBeenCalled()
  })

  it('reads the flag from the route, and treats an unflagged one as not live', () => {
    expect(holdsLiveSession({ meta: { liveSession: true } })).toBe(true)
    expect(holdsLiveSession({ meta: {} })).toBe(false)
  })
})

// The REAL route records, so a page that gains or loses its flag fails here
// and not on a live session in the field.
describe('the routes that hold a live session (#2841)', () => {
  const flag = (name: string) => {
    const record = realRouter.getRoutes().find((r) => r.name === name)
    expect(record, `route ${name}`).toBeDefined()
    return record?.meta.liveSession === true
  }

  it('flags both remote viewers and the call', () => {
    expect(flag('agent-remote')).toBe(true)
    expect(flag('connect-session')).toBe(true)
    expect(flag('room-call')).toBe(true)
  })

  it('leaves the pages a link may safely replace unflagged', () => {
    for (const name of ['consent', 'room-chat', 'devices', 'tenant-dashboard', 'connect']) {
      expect(flag(name), name).toBe(false)
    }
  })
})
