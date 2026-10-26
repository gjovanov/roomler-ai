// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #2936 — the tray's "Check for updates" is ONE click: the page switches to
 * Overview, brings the Updates card into view and runs the same check as the
 * card's own button. Before, the tray showed the window at the top of
 * Overview with the result in the last card, and people scrolled down and
 * clicked "Check for updates" a second time.
 *
 * Field 2026-10-26 on 0.4.177 (a Mac): the check ran, but the card did not
 * come into view, and an answer that arrived at once ("Up to date") read as
 * no check at all. Hence: an instant scroll, again when the page becomes
 * visible and when the answer lands; "Checking…" held for a moment; the
 * answer stamped with its time; the card ringed once.
 *
 * Driven by the REAL Overview section of `index.html` and `overview.js`, in
 * jsdom, against a mocked Tauri `invoke` (the Recordings view's approach).
 * jsdom has no layout, so WHERE the card lands is measured separately, in
 * real engines (the PR records it); this pins WHAT is asked, and when.
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
// overview.js's MIN_CHECKING_MS.
const MIN_CHECKING_MS = 700

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
let visibility: DocumentVisibilityState = 'visible'

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
const answered = () => vi.advanceTimersByTimeAsync(MIN_CHECKING_MS)
const checks = (invoke: ReturnType<typeof vi.fn>) =>
  invoke.mock.calls.filter((c) => c[0] === 'cmd_check_update').length
const output = () => $('action-output').textContent ?? ''
const checkButton = () => $('btn-check-update') as HTMLButtonElement

// The answer, then the time it was asked on its own line.
function expectAnswer(text: string) {
  const [first, second] = output().split('\n')
  expect(first).toBe(text)
  expect(second).toMatch(/^Checked at .+\.$/)
}

describe('companion: the tray\'s "Check for updates" in one click (#2936)', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    trackListeners(window)
    trackListeners(document)
    scrolled = []
    visibility = 'visible'
    Object.defineProperty(document, 'visibilityState', {
      configurable: true,
      get: () => visibility,
    })
    Element.prototype.scrollIntoView = function (this: Element) {
      scrolled.push(this.id)
    } as Element['scrollIntoView']
  })
  afterEach(() => {
    for (const [target, type, listener] of added.splice(0)) {
      target.removeEventListener(type, listener)
    }
    for (const [target, orig] of originals.splice(0)) target.addEventListener = orig
    delete (document as unknown as Record<string, unknown>).visibilityState
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
    expect($('ov-updates').classList.contains('flash')).toBe(true)
    expect(checks(invoke)).toBe(1)
    expect(output()).toBe('Checking for updates…')

    await answered()
    expectAnswer(AVAILABLE)
    expect($('btn-apply-update').hidden).toBe(false)
    // In view again once the answer lands: the cards above may have grown.
    expect(scrolled).toEqual(['ov-updates', 'ov-updates'])
    expect((window as unknown as Win).__roomler_tray_check_pending).toBe(false)
  })

  it('holds "Checking…" for a moment even when the answer is instant, then says when it checked', async () => {
    const invoke = invokeWith(async () => UP_TO_DATE)
    mount(invoke, { view: 'overview' })
    trayClick()
    await settle()
    expect(output()).toBe('Checking for updates…')
    expect(checkButton().disabled).toBe(true)

    await vi.advanceTimersByTimeAsync(MIN_CHECKING_MS - 1)
    expect(output()).toBe('Checking for updates…')
    await vi.advanceTimersByTimeAsync(1)
    expectAnswer(UP_TO_DATE)
    expect(checkButton().disabled).toBe(false)
    expect($('btn-apply-update').hidden).toBe(true)
  })

  it('a slow answer is shown as soon as it lands', async () => {
    let answer!: (v: string) => void
    const invoke = invokeWith(() => new Promise((r) => (answer = r)))
    mount(invoke, { view: 'overview' })
    trayClick()
    await vi.advanceTimersByTimeAsync(5000)
    expect(output()).toBe('Checking for updates…')
    answer(UP_TO_DATE)
    await settle()
    expectAnswer(UP_TO_DATE)
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

  it('a page that is not visible yet is placed again the moment it is', async () => {
    let answer!: (v: string) => void
    const invoke = invokeWith(() => new Promise((r) => (answer = r)))
    mount(invoke, { view: 'overview' })
    visibility = 'hidden'
    trayClick()
    await settle()
    expect(scrolled).toEqual(['ov-updates'])

    visibility = 'visible'
    document.dispatchEvent(new Event('visibilitychange'))
    expect(scrolled).toEqual(['ov-updates', 'ov-updates'])
    // Once is enough: a later change does not move the page again.
    document.dispatchEvent(new Event('visibilitychange'))
    expect(scrolled).toEqual(['ov-updates', 'ov-updates'])

    answer(UP_TO_DATE)
    await answered()
    expect(scrolled).toEqual(['ov-updates', 'ov-updates', 'ov-updates'])
  })

  it('rings the card once, and a second tray click rings it again', async () => {
    const invoke = invokeWith(async () => UP_TO_DATE)
    mount(invoke, { view: 'overview' })
    trayClick()
    await settle()
    expect($('ov-updates').classList.contains('flash')).toBe(true)
    await vi.advanceTimersByTimeAsync(1600)
    expect($('ov-updates').classList.contains('flash')).toBe(false)

    trayClick()
    await settle()
    expect($('ov-updates').classList.contains('flash')).toBe(true)
  })

  it('a click that landed before the page listened runs when the script starts', async () => {
    const invoke = invokeWith(async () => AVAILABLE)
    mount(invoke, { pending: true })
    await settle()
    await settle()
    expect(checks(invoke)).toBe(1)
    expect(scrolled).toEqual(['ov-updates'])
    await answered()
    expectAnswer(AVAILABLE)
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

  it('the card\'s button runs the same check without moving the page, and a re-check hides a stale "Install update"', async () => {
    let next = AVAILABLE
    const invoke = invokeWith(async () => next)
    mount(invoke, { view: 'overview' })
    checkButton().click()
    await answered()
    expectAnswer(AVAILABLE)
    expect($('btn-apply-update').hidden).toBe(false)

    next = UP_TO_DATE
    checkButton().click()
    await settle()
    expect($('btn-apply-update').hidden).toBe(true)
    await answered()
    expect(checks(invoke)).toBe(2)
    expectAnswer(UP_TO_DATE)
    // The person is already looking at the card they clicked in.
    expect(scrolled).toEqual([])
  })

  it('a second click while a check runs is ignored', async () => {
    let answer!: (v: string) => void
    const invoke = invokeWith(() => new Promise((r) => (answer = r)))
    mount(invoke, { view: 'overview' })
    checkButton().click()
    checkButton().click()
    trayClick()
    await settle()
    expect(checks(invoke)).toBe(1)
    answer(AVAILABLE)
    await answered()
    expect($('btn-apply-update').hidden).toBe(false)
  })

  it('a failed check says so in the card, with its time', async () => {
    const invoke = invokeWith(async () => {
      throw 'self-update --check-only exited Some(1): no route to the server'
    })
    mount(invoke, { view: 'overview' })
    trayClick()
    await answered()
    expectAnswer('Error: self-update --check-only exited Some(1): no route to the server')
    expect($('btn-apply-update').hidden).toBe(true)
    expect(checkButton().disabled).toBe(false)
  })
})
