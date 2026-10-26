// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #2936 — the tray's "Check for updates" is ONE click: the page switches to
 * Overview, brings the Updates card into view and runs the same check as the
 * card's own button. Before, the tray showed the window at the top of
 * Overview with the result in the last card, and people scrolled down and
 * clicked "Check for updates" a second time.
 *
 * Driven by the REAL Overview section of `index.html` and `overview.js`, in
 * jsdom, against a mocked Tauri `invoke` (the Recordings view's approach).
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = dirname(fileURLToPath(import.meta.url))
const FRONT = join(HERE, '..', '..', '..', '..', 'agents', 'roomler-desktop', 'src', 'front')
const HTML = readFileSync(join(FRONT, 'index.html'), 'utf8')
const SCRIPT = readFileSync(join(FRONT, 'overview.js'), 'utf8')

const AVAILABLE = 'Update available: 0.4.172 -> agent-v0.4.174'
const UP_TO_DATE = 'Up to date (current: 0.4.174, latest: agent-v0.4.174)'

type Invoke = (name: string, payload?: Record<string, unknown>) => Promise<unknown>
type Win = Record<string, unknown>

// Each mount evaluates the script again, so the listeners it adds are
// tracked and removed after the test; otherwise an earlier mount's handlers
// would run a second check against the next test's page.
const added: Array<[EventTarget, string, EventListenerOrEventListenerObject]> = []
const originals: Array<[EventTarget, EventTarget['addEventListener']]> = []
function trackListeners(target: EventTarget) {
  const orig = target.addEventListener
  originals.push([target, orig])
  target.addEventListener = function (
    this: EventTarget,
    type: string,
    listener: EventListenerOrEventListenerObject | null,
    options?: boolean | AddEventListenerOptions,
  ) {
    if (listener) added.push([target, type, listener])
    return orig.call(this, type, listener, options)
  } as EventTarget['addEventListener']
}

let scrolled: string[] = []

function mount(invoke: Invoke, opts: { view?: 'overview' | 'devices'; pending?: boolean } = {}) {
  const parsed = new DOMParser().parseFromString(HTML, 'text/html')
  document.body.innerHTML = ''
  // Two views, so the switch to Overview is observable.
  for (const id of ['view-overview', 'view-devices']) {
    document.body.appendChild(document.importNode(parsed.getElementById(id)!, true))
  }
  let current: string = opts.view ?? 'devices'
  // app.js's applyRoute: unhide one view, then announce it.
  const applyRoute = () => {
    for (const v of ['overview', 'devices']) {
      document.getElementById('view-' + v)!.hidden = v !== current
    }
    document.dispatchEvent(new CustomEvent('roomler:view', { detail: current }))
  }
  applyRoute()
  // app.js sets `location.hash`; the route applies on the later `hashchange`.
  const navigate = vi.fn((view: string) => {
    current = view
    setTimeout(applyRoute, 0)
  })
  const w = window as unknown as Win
  w.Roomler = {
    $: (id: string) => document.getElementById(id),
    invoke,
    show: (el: HTMLElement | null) => el && (el.hidden = false),
    hide: (el: HTMLElement | null) => el && (el.hidden = true),
    setText: (id: string, t: string) => {
      const el = document.getElementById(id)
      if (el) el.textContent = t
    },
    on: () => {},
    get: () => undefined,
    navigate,
    currentView: () => current,
  }
  w.__roomler_tray_check_pending = opts.pending === true
  window.eval(SCRIPT)
  document.dispatchEvent(new Event('DOMContentLoaded'))
  return { navigate }
}

// Exactly what tray.rs evaluates (`tray_check_update_js`).
function trayClick() {
  const w = window as unknown as Win
  w.__roomler_tray_check_pending = true
  window.dispatchEvent(new Event('roomler-tray-check-update'))
}

// Only the update check answers; every other command fails, which the
// Overview's other cards already handle.
function invokeWith(check: () => Promise<unknown>) {
  return vi.fn(async (name: string) => {
    if (name === 'cmd_check_update') return check()
    throw new Error('not mocked: ' + name)
  })
}

const $ = (id: string) => document.getElementById(id) as HTMLElement
const settle = () => vi.advanceTimersByTimeAsync(0)
const checks = (invoke: ReturnType<typeof vi.fn>) =>
  invoke.mock.calls.filter((c) => c[0] === 'cmd_check_update').length

describe('companion: the tray\'s "Check for updates" in one click (#2936)', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    trackListeners(window)
    trackListeners(document)
    scrolled = []
    Element.prototype.scrollIntoView = function (this: Element) {
      scrolled.push(this.id)
    } as Element['scrollIntoView']
  })
  afterEach(() => {
    for (const [target, type, listener] of added.splice(0)) {
      target.removeEventListener(type, listener)
    }
    for (const [target, orig] of originals.splice(0)) target.addEventListener = orig
    vi.clearAllTimers()
    vi.useRealTimers()
  })

  it('switches to Overview, brings the Updates card into view and runs the check', async () => {
    const invoke = invokeWith(async () => AVAILABLE)
    const { navigate } = mount(invoke)
    await settle()
    expect($('view-overview').hidden).toBe(true)

    trayClick()
    await settle()
    await settle()

    expect(navigate).toHaveBeenCalledWith('overview')
    expect($('view-overview').hidden).toBe(false)
    expect(scrolled).toEqual(['ov-updates'])
    expect(checks(invoke)).toBe(1)
    expect($('action-output').textContent).toBe(AVAILABLE)
    expect($('action-output').hidden).toBe(false)
    expect($('btn-apply-update').hidden).toBe(false)
    expect((window as unknown as Win).__roomler_tray_check_pending).toBe(false)
  })

  it('says it is checking while the check runs', async () => {
    let answer!: (v: string) => void
    const invoke = invokeWith(() => new Promise((r) => (answer = r)))
    mount(invoke, { view: 'overview' })
    trayClick()
    await settle()
    expect($('action-output').textContent).toBe('Checking for updates…')
    answer(UP_TO_DATE)
    await settle()
    expect($('action-output').textContent).toBe(UP_TO_DATE)
    expect($('btn-apply-update').hidden).toBe(true)
  })

  it('already on Overview: scrolls to the card without navigating', async () => {
    const invoke = invokeWith(async () => UP_TO_DATE)
    const { navigate } = mount(invoke, { view: 'overview' })
    trayClick()
    await settle()
    expect(navigate).not.toHaveBeenCalled()
    expect(scrolled).toEqual(['ov-updates'])
    expect(checks(invoke)).toBe(1)
  })

  it('a click that landed before the page listened runs when the script starts', async () => {
    const invoke = invokeWith(async () => AVAILABLE)
    mount(invoke, { pending: true })
    await settle()
    await settle()
    expect(checks(invoke)).toBe(1)
    expect(scrolled).toEqual(['ov-updates'])
    expect($('btn-apply-update').hidden).toBe(false)
    expect((window as unknown as Win).__roomler_tray_check_pending).toBe(false)
  })

  it('no pending click: the page runs no check on its own', async () => {
    const invoke = invokeWith(async () => AVAILABLE)
    mount(invoke)
    await settle()
    expect(checks(invoke)).toBe(0)
    expect($('action-output').hidden).toBe(true)
  })

  it('the card\'s button runs the same check, and a re-check hides a stale "Install update"', async () => {
    let next = AVAILABLE
    const invoke = invokeWith(async () => next)
    mount(invoke, { view: 'overview' })
    $('btn-check-update').click()
    await settle()
    expect($('btn-apply-update').hidden).toBe(false)

    next = UP_TO_DATE
    $('btn-check-update').click()
    await settle()
    expect(checks(invoke)).toBe(2)
    expect($('action-output').textContent).toBe(UP_TO_DATE)
    expect($('btn-apply-update').hidden).toBe(true)
  })

  it('a second click while a check runs is ignored', async () => {
    let answer!: (v: string) => void
    const invoke = invokeWith(() => new Promise((r) => (answer = r)))
    mount(invoke, { view: 'overview' })
    $('btn-check-update').click()
    $('btn-check-update').click()
    trayClick()
    await settle()
    expect(checks(invoke)).toBe(1)
    answer(AVAILABLE)
    await settle()
    expect($('btn-apply-update').hidden).toBe(false)
  })

  it('a failed check says so in the card', async () => {
    const invoke = invokeWith(async () => {
      throw 'self-update --check-only exited Some(1): no route to the server'
    })
    mount(invoke, { view: 'overview' })
    trayClick()
    await settle()
    expect($('action-output').textContent).toBe(
      'Error: self-update --check-only exited Some(1): no route to the server',
    )
    expect($('btn-apply-update').hidden).toBe(true)
  })
})
