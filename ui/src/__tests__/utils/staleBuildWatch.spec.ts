// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3582 — the watch around the policy: it reads at the socket's (re)connect
// and on its own clock, acts only on a confirmed mismatch, never reloads
// where it cannot keep count, hands a redialing session over to the next
// page load, and resumes it there once.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ref } from 'vue'
import {
  BUSY_MS,
  HIDDEN_IDLE_MS,
  READ_EVERY_MS,
  RELOAD_GIVE_UP_MS,
  TICK_MS,
  createStaleBuildWatch,
  type NewBuildBanner,
  type StaleBuildWatchDeps,
} from '@/utils/staleBuildWatch'
import {
  NEW_BUILD_RELOAD_KEY,
  RC_RESUME_KEY,
  writeRcResumeIntent,
  type RcReloadState,
  type RcReloadSurface,
} from '@/utils/staleBuildPolicy'

const OWN = 'aaaaaaaaaaaaaaaa'
const NEW = 'bbbbbbbbbbbbbbbb'
const AGENT = '69f0000000000000000000c3'
const TENANT = '69f0000000000000000000a1'
const PATH = `/tenant/${TENANT}/agent/${AGENT}/remote`
const ARGS = { agentId: AGENT, permissions: 'VIEW | INPUT | CLIPBOARD | FILES | RECORD', orgId: TENANT }
const USER = '69f0000000000000000000b7'

function memoryStorage(opts: { readonly?: boolean } = {}): Storage {
  const m = new Map<string, string>()
  return {
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => {
      if (opts.readonly) throw new Error('QuotaExceededError')
      m.set(k, v)
    },
    removeItem: (k: string) => void m.delete(k),
    clear: () => m.clear(),
    key: () => null,
    get length() {
      return m.size
    },
  } as Storage
}

function surface(initial: RcReloadState['session'], over: Partial<RcReloadState> = {}) {
  const st: RcReloadState = { session: initial, external: false, work: false, hold: false, asksConsent: false, ...over }
  const s = {
    st,
    holdRedial: vi.fn(() => (st.session === 'redialing' ? { ...ARGS } : null)),
    releaseRedial: vi.fn(),
    resumeArgs: vi.fn(() => (st.external || st.session === 'none' ? null : { ...ARGS })),
    userId: () => USER,
    state: () => ({ ...st }),
  }
  return s as typeof s & RcReloadSurface
}

function harness(
  opts: {
    own?: string | null
    served?: string | null | ((n: number) => string | null)
    storage?: Storage | 'throws'
    online?: boolean
    ws?: string
    meeting?: boolean
  } = {},
) {
  const listeners = new Map<string, Array<(e: Event) => void>>()
  const doc = {
    visibilityState: 'visible' as DocumentVisibilityState,
    querySelector: (sel: string) => document.querySelector(sel),
    addEventListener: (type: string, fn: (e: Event) => void) => {
      if (!listeners.has(type)) listeners.set(type, [])
      listeners.get(type)!.push(fn)
    },
  }
  const location = { reload: vi.fn(), pathname: PATH }
  const store = opts.storage === 'throws' ? undefined : (opts.storage ?? memoryStorage())
  let n = 0
  const read = vi.fn(async () => {
    const s = opts.served === undefined ? NEW : opts.served
    return typeof s === 'function' ? s(n++) : s
  })
  const banner: NewBuildBanner = { visible: false, hint: null }
  const status = ref(opts.ws ?? 'connected')
  const meeting = ref(opts.meeting ?? false)
  const uploading = ref(false)
  const watchDeps: StaleBuildWatchDeps = {
    own: opts.own === undefined ? OWN : opts.own,
    document: doc as unknown as StaleBuildWatchDeps['document'],
    navigator: { onLine: opts.online ?? true },
    location,
    storage: () => {
      if (opts.storage === 'throws') throw new Error('SecurityError')
      return store
    },
    read,
    monoNow: () => Date.now(),
    wallNow: () => Date.now(),
    banner,
    log: () => {},
  }
  const watch = createStaleBuildWatch(watchDeps)
  const unbind = watch.bind({
    wsStatus: () => status.value,
    meetingLive: () => meeting.value,
    uploading: () => uploading.value,
  })
  return {
    watch,
    unbind,
    doc,
    location,
    read,
    banner,
    status,
    meeting,
    uploading,
    store,
    fire: (type: string, e: Partial<Event> = {}) => listeners.get(type)?.forEach((fn) => fn(e as Event)),
  }
}

/** Let the read the bind started settle, then let `ms` pass. */
async function passes(ms: number) {
  await vi.advanceTimersByTimeAsync(ms)
}

beforeEach(() => {
  vi.useFakeTimers({ now: 1_000_000 })
})

afterEach(() => {
  vi.useRealTimers()
  document.body.innerHTML = ''
})

describe('when the page reads, and what one read is worth', () => {
  it('reads at the socket (re)connect; a first mismatch alone never reloads', async () => {
    const h = harness()
    await passes(0)
    expect(h.read).toHaveBeenCalledTimes(1)
    expect(h.watch.track.pending?.build).toBe(NEW)
    await passes(20_000)
    expect(h.location.reload).not.toHaveBeenCalled()
    expect(h.banner.visible).toBe(false)
  })

  it('reads again 31 s after a first mismatch, and a quiet page then reloads, once', async () => {
    const h = harness()
    await passes(31_000)
    expect(h.read).toHaveBeenCalledTimes(2)
    expect(h.watch.track.confirmed).toBe(NEW)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
    expect(h.store!.getItem(NEW_BUILD_RELOAD_KEY)).toBe(String(Date.now()))
    await passes(5 * TICK_MS)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })

  it('a roll serving both builds never confirms, so never reloads', async () => {
    const h = harness({ served: (i) => (i % 2 === 0 ? NEW : OWN) })
    await passes(0)
    for (let i = 0; i < 6; i++) {
      h.status.value = 'disconnected'
      await passes(1_000)
      h.status.value = 'connected'
      await passes(40_000)
    }
    expect(h.watch.track.confirmed).toBeNull()
    expect(h.location.reload).not.toHaveBeenCalled()
  })

  it('reads every 10 minutes while visible, never while hidden', async () => {
    const h = harness({ served: OWN })
    await passes(0)
    expect(h.read).toHaveBeenCalledTimes(1)
    await passes(READ_EVERY_MS + TICK_MS)
    expect(h.read).toHaveBeenCalledTimes(2)
    h.doc.visibilityState = 'hidden'
    h.fire('visibilitychange')
    await passes(3 * READ_EVERY_MS)
    expect(h.read).toHaveBeenCalledTimes(2)
    h.doc.visibilityState = 'visible'
    h.fire('visibilitychange')
    await passes(0)
    expect(h.read).toHaveBeenCalledTimes(3)
  })

  it('offline: no read at all', async () => {
    const h = harness({ online: false })
    await passes(READ_EVERY_MS * 2)
    expect(h.read).not.toHaveBeenCalled()
  })

  it('a page with no build id (the dev server, an older shell) never reads', async () => {
    const h = harness({ own: null })
    await passes(READ_EVERY_MS * 2)
    expect(h.read).not.toHaveBeenCalled()
  })
})

describe('the guards', () => {
  it('a failed confirming read waits 30 s, then backs off: never a loop (a 5xx, a captive portal, an image with no such file)', async () => {
    const h = harness({ served: (i) => (i === 0 ? NEW : null) })
    await passes(0)
    expect(h.read).toHaveBeenCalledTimes(1) // the new build: pending
    await passes(31_000)
    expect(h.read).toHaveBeenCalledTimes(2) // the confirming read fails
    await passes(29_000)
    expect(h.read).toHaveBeenCalledTimes(2) // nothing for 30 s after it
    await passes(2_000)
    expect(h.read).toHaveBeenCalledTimes(3) // then once, and it fails again
    await passes(10 * 60_000)
    // 60 s, 120 s, 240 s, 480 s: a handful, where the loop read without end.
    expect(h.read.mock.calls.length).toBeLessThanOrEqual(7)
    expect(h.watch.track.confirmed).toBeNull()
    expect(h.location.reload).not.toHaveBeenCalled()
  })

  it('a socket that is down is not steady, however long it has been down', async () => {
    const h = harness()
    await passes(0)
    h.status.value = 'disconnected'
    await passes(5 * 60_000)
    // The reads do not need the socket, so the new build is confirmed…
    expect(h.watch.track.confirmed).toBe(NEW)
    // …but a page whose own socket is down may be looking at a roll: no reload.
    expect(h.location.reload).not.toHaveBeenCalled()
    expect(h.banner.visible).toBe(true)
  })

  it('where storage cannot be written, nothing reloads: the banner offers it instead', async () => {
    const h = harness({ storage: memoryStorage({ readonly: true }) })
    await passes(31_000)
    expect(h.watch.track.confirmed).toBe(NEW)
    expect(h.location.reload).not.toHaveBeenCalled()
    expect(h.banner.visible).toBe(true)
  })

  it('where storage cannot even be read, nothing reloads either', async () => {
    const h = harness({ storage: 'throws' })
    await passes(31_000)
    expect(h.location.reload).not.toHaveBeenCalled()
  })

  it('one automatic reload in 10 minutes: a page still stale after one shows the banner', async () => {
    const storage = memoryStorage()
    storage.setItem(NEW_BUILD_RELOAD_KEY, String(1_000_000 - 60_000))
    const h = harness({ storage })
    await passes(31_000)
    expect(h.location.reload).not.toHaveBeenCalled()
    expect(h.banner.visible).toBe(true)
  })

  it('a socket that just came back is not settled: the reload waits 30 s past it', async () => {
    const h = harness()
    await passes(20_000)
    h.status.value = 'disconnected'
    await passes(500)
    h.status.value = 'connected'
    await passes(11_000) // the confirming read at 31 s, 10.5 s after the reconnect
    expect(h.watch.track.confirmed).toBe(NEW)
    expect(h.location.reload).not.toHaveBeenCalled()
    await passes(30_000)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })

  it("waits for the person's hands to rest", async () => {
    const h = harness()
    await passes(30_000)
    h.fire('keydown')
    await passes(1_000) // the confirming read lands while a key was just pressed
    expect(h.watch.track.confirmed).toBe(NEW)
    expect(h.location.reload).not.toHaveBeenCalled()
    await passes(BUSY_MS + TICK_MS)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })
})

describe('what a reload would lose', () => {
  it('a key shown once (data-unsaved) still holds after the page has sat hidden and idle for 30 minutes', async () => {
    const h = harness()
    const key = document.createElement('div')
    key.setAttribute('data-unsaved', '')
    document.body.appendChild(key)
    h.doc.visibilityState = 'hidden'
    h.fire('visibilitychange')
    await passes(31_000 + HIDDEN_IDLE_MS + 2 * TICK_MS)
    expect(h.watch.track.confirmed).toBe(NEW)
    expect(h.location.reload).not.toHaveBeenCalled()
  })

  it('typed text, unlike a hold, may go once the page has sat hidden with no input for 30 minutes', async () => {
    const h = harness()
    const box = document.createElement('textarea')
    document.body.appendChild(box)
    box.value = 'half a message'
    h.fire('input', { target: box })
    h.doc.visibilityState = 'hidden'
    h.fire('visibilitychange')
    await passes(31_000 + TICK_MS)
    expect(h.location.reload).not.toHaveBeenCalled()
    await passes(HIDDEN_IDLE_MS)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })

  it('an upload in flight holds the page until it ends', async () => {
    const h = harness()
    h.uploading.value = true
    await passes(31_000 + TICK_MS)
    expect(h.location.reload).not.toHaveBeenCalled()
    h.uploading.value = false
    await passes(TICK_MS)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })

  it('typed text waits; once it is sent, the page reloads', async () => {
    const h = harness()
    const box = document.createElement('textarea')
    document.body.appendChild(box)
    box.value = 'half a message'
    h.fire('input', { target: box })
    await passes(31_000 + BUSY_MS)
    expect(h.location.reload).not.toHaveBeenCalled()
    expect(h.banner.visible).toBe(true)
    box.value = ''
    await passes(TICK_MS)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })

  it("what is typed to the remote host is the host's, not a draft", async () => {
    const h = harness()
    const keyboard = document.createElement('textarea')
    keyboard.setAttribute('data-rc-capture', 'soft-keyboard')
    document.body.appendChild(keyboard)
    keyboard.value = 'ls -la'
    h.fire('input', { target: keyboard })
    await passes(31_000 + BUSY_MS + TICK_MS)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })

  it('an open dialog, or anything marked data-unsaved, waits', async () => {
    const h = harness()
    const dialog = document.createElement('div')
    dialog.className = 'v-overlay v-overlay--active v-dialog'
    document.body.appendChild(dialog)
    await passes(31_000 + TICK_MS)
    expect(h.location.reload).not.toHaveBeenCalled()
    dialog.remove()
    const key = document.createElement('div')
    key.setAttribute('data-unsaved', '')
    document.body.appendChild(key)
    await passes(TICK_MS)
    expect(h.location.reload).not.toHaveBeenCalled()
    key.remove()
    await passes(TICK_MS)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })

  it('a call waits until it ends, and the banner says Reload leaves it', async () => {
    const h = harness({ meeting: true })
    await passes(31_000 + TICK_MS)
    expect(h.location.reload).not.toHaveBeenCalled()
    expect(h.banner).toEqual({ visible: true, hint: 'call' })
    h.meeting.value = false
    await passes(TICK_MS)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })
})

describe('the remote-control session', () => {
  it('a live session shows the banner; at its next cut the reload takes over the redial, and nothing is lost', async () => {
    const h = harness()
    const rc = surface('live')
    h.watch.registerSurface(rc)
    await passes(31_000 + TICK_MS)
    expect(h.location.reload).not.toHaveBeenCalled()
    expect(h.banner).toEqual({ visible: true, hint: 'rcResume' })

    rc.st.session = 'redialing'
    h.watch.surfaceChanged()
    expect(rc.holdRedial).toHaveBeenCalledTimes(1)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
    expect(rc.releaseRedial).not.toHaveBeenCalled()
    expect(JSON.parse(h.store!.getItem(RC_RESUME_KEY)!)).toEqual({ v: 1, ...ARGS, at: Date.now(), path: PATH, userId: USER })
  })

  it('a reload that did not happen gives the redial back, and drops the resume it recorded', async () => {
    const h = harness()
    const rc = surface('redialing')
    h.watch.registerSurface(rc)
    await passes(31_000)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
    expect(h.store!.getItem(RC_RESUME_KEY)).not.toBeNull()
    await passes(RELOAD_GIVE_UP_MS)
    expect(rc.releaseRedial).toHaveBeenCalledTimes(1)
    // Left there, an F5 inside the minute would open the session a second
    // time beside the redial, and on a device that asks its user, ask twice.
    expect(h.store!.getItem(RC_RESUME_KEY)).toBeNull()
    // The claim is spent: it does not try again, it shows the banner.
    rc.st.session = 'live'
    await passes(TICK_MS)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })

  it('on a live session whose device asks someone to allow it, the banner says it will ask again', async () => {
    const h = harness()
    h.watch.registerSurface(surface('live', { asksConsent: true }))
    await passes(31_000)
    expect(h.banner).toEqual({ visible: true, hint: 'rcResumeAsks' })
  })

  it('a session that ended a moment ago holds the page: its notice, its error, the rating prompt', async () => {
    const h = harness()
    const rc = surface('none', { hold: true })
    h.watch.registerSurface(rc)
    await passes(31_000 + TICK_MS)
    expect(h.location.reload).not.toHaveBeenCalled()
    rc.st.hold = false
    await passes(TICK_MS)
    expect(h.location.reload).toHaveBeenCalledTimes(1)
  })

  it('a redial carrying a transfer only this page holds waits', async () => {
    const h = harness()
    const rc = surface('redialing', { work: true })
    h.watch.registerSurface(rc)
    await passes(31_000 + TICK_MS)
    expect(rc.holdRedial).not.toHaveBeenCalled()
    expect(h.location.reload).not.toHaveBeenCalled()
  })

  it('an external session is never reloaded under, and never resumed', async () => {
    const h = harness()
    const rc = surface('live', { external: true })
    h.watch.registerSurface(rc)
    await passes(31_000 + TICK_MS)
    expect(h.location.reload).not.toHaveBeenCalled()
    expect(h.banner).toEqual({ visible: true, hint: 'rcExternal' })
    h.watch.reloadNow()
    expect(h.location.reload).toHaveBeenCalledTimes(1)
    expect(h.store!.getItem(RC_RESUME_KEY)).toBeNull()
  })

  it("the banner's Reload resumes a live session, and counts as no automatic reload", async () => {
    const h = harness()
    const rc = surface('live')
    h.watch.registerSurface(rc)
    await passes(31_000)
    h.watch.reloadNow()
    expect(h.location.reload).toHaveBeenCalledTimes(1)
    expect(JSON.parse(h.store!.getItem(RC_RESUME_KEY)!).agentId).toBe(AGENT)
    expect(h.store!.getItem(NEW_BUILD_RELOAD_KEY)).toBeNull()
  })

  it('Later hides the banner until a newer build is confirmed', async () => {
    const h = harness()
    h.watch.registerSurface(surface('live'))
    await passes(31_000)
    expect(h.banner.visible).toBe(true)
    h.watch.dismiss()
    await passes(TICK_MS)
    expect(h.banner.visible).toBe(false)
  })
})

describe('the resume, on the next page load', () => {
  function seeded(at: number) {
    const storage = memoryStorage()
    writeRcResumeIntent(storage, ARGS, at, PATH, USER)
    return storage
  }

  it('is taken out of storage at once, and claimed once by the first page the load renders', () => {
    const storage = seeded(Date.now() - 2_000)
    const h = harness({ storage, served: OWN })
    expect(storage.getItem(RC_RESUME_KEY)).toBeNull()
    h.watch.navigationCompleted()
    expect(h.watch.claimRcResume({ agentId: AGENT, tenantId: TENANT, path: PATH, userId: USER })).toEqual(ARGS)
    expect(h.watch.claimRcResume({ agentId: AGENT, tenantId: TENANT, path: PATH, userId: USER })).toBeNull()
  })

  it('not after another page rendered first (a sign-in in between), and not a minute later', () => {
    const h1 = harness({ storage: seeded(Date.now() - 2_000), served: OWN })
    h1.watch.navigationCompleted()
    h1.watch.navigationCompleted()
    expect(h1.watch.claimRcResume({ agentId: AGENT, tenantId: TENANT, path: PATH, userId: USER })).toBeNull()

    const h2 = harness({ storage: seeded(Date.now() - 60_000), served: OWN })
    h2.watch.navigationCompleted()
    expect(h2.watch.claimRcResume({ agentId: AGENT, tenantId: TENANT, path: PATH, userId: USER })).toBeNull()
  })

  it('not for another person signed in on this tab, nor before anyone is', () => {
    const h1 = harness({ storage: seeded(Date.now() - 2_000), served: OWN })
    h1.watch.navigationCompleted()
    expect(h1.watch.claimRcResume({ agentId: AGENT, tenantId: TENANT, path: PATH, userId: '69f0000000000000000000dd' })).toBeNull()

    const h2 = harness({ storage: seeded(Date.now() - 2_000), served: OWN })
    h2.watch.navigationCompleted()
    expect(h2.watch.claimRcResume({ agentId: AGENT, tenantId: TENANT, path: PATH, userId: null })).toBeNull()
  })
})
