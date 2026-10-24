// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect, vi } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

// The service worker is a plain script in public/: load it into a fake worker
// scope and drive its listeners directly.
type Listener = (event: unknown) => void
const SOURCE = readFileSync(resolve(__dirname, '../../public/sw.js'), 'utf8')
const ORIGIN = 'https://roomler.ai'

function loadWorker(windows: { url: string }[]) {
  const listeners: Record<string, Listener> = {}
  const clientObjs = windows.map((w) => ({ url: w.url, focus: vi.fn(async () => undefined), navigate: vi.fn() }))
  const clients = {
    matchAll: vi.fn(async () => clientObjs),
    openWindow: vi.fn(async () => undefined),
    claim: vi.fn(async () => undefined),
  }
  const self = {
    location: { origin: ORIGIN },
    addEventListener: (type: string, fn: Listener) => {
      listeners[type] = fn
    },
    skipWaiting: vi.fn(),
    clients,
    registration: { showNotification: vi.fn() },
  }
  new Function('self', 'clients', SOURCE)(self, clients)
  return { listeners, clients, clientObjs, self }
}

async function click(listeners: Record<string, Listener>, url: string) {
  let pending: Promise<unknown> = Promise.resolve()
  listeners.notificationclick!({
    notification: { close: vi.fn(), data: { url } },
    waitUntil: (p: Promise<unknown>) => {
      pending = p
    },
  })
  await pending
}

describe('#2827 a push notification click never navigates an open window away', () => {
  it('opens the link in a NEW window when no window is on that page', async () => {
    // The field case: the only roomler window is the controller's own viewer.
    const viewer = `${ORIGIN}/tenant/t/agent/a/remote`
    const { listeners, clients, clientObjs } = loadWorker([{ url: viewer }])
    await click(listeners, `${ORIGIN}/consent/tok`)
    expect(clientObjs[0]!.navigate).not.toHaveBeenCalled()
    expect(clientObjs[0]!.focus).not.toHaveBeenCalled()
    expect(clients.openWindow).toHaveBeenCalledWith(`${ORIGIN}/consent/tok`)
  })

  it('focuses a window that is already on that page instead of opening another', async () => {
    const page = `${ORIGIN}/consent/tok`
    const { listeners, clients, clientObjs } = loadWorker([{ url: `${ORIGIN}/` }, { url: page }])
    await click(listeners, page)
    expect(clientObjs[1]!.focus).toHaveBeenCalledOnce()
    expect(clientObjs[0]!.navigate).not.toHaveBeenCalled()
    expect(clients.openWindow).not.toHaveBeenCalled()
  })

  it('resolves a relative link against the origin', async () => {
    const { listeners, clients } = loadWorker([])
    await click(listeners, '/tenant/t/room/r')
    expect(clients.openWindow).toHaveBeenCalledWith(`${ORIGIN}/tenant/t/room/r`)
  })

  it('takes over at once: it caches nothing, so an old click handler must not linger', async () => {
    const { listeners, self, clients } = loadWorker([])
    listeners.install!({})
    expect(self.skipWaiting).toHaveBeenCalledOnce()
    let pending: Promise<unknown> = Promise.resolve()
    listeners.activate!({ waitUntil: (p: Promise<unknown>) => { pending = p } })
    await pending
    expect(clients.claim).toHaveBeenCalledOnce()
  })
})
