// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #3582 — the wiring of `staleBuildPolicy.ts`: when a page reads which build
 * the server serves, what it looks at to decide, and the reload.
 *
 * A tab that never navigates kept the build it loaded through any number of
 * promotes: the `/ws` reconnects in place, the remote-control page opens its
 * next session by itself, and `utils/staleBuild.ts` fires only on a failed
 * chunk load. One controller's viewer ran a pre-November build through three
 * promotes, so viewer fixes never reached the heaviest users.
 *
 * Reads (`readServedBuild`, `utils/buildId.ts`):
 * - at every `/ws` (re)connect, the first sign of a roll. AppLayout owns the
 *   socket and binds it here (`bindStaleBuildWatch`);
 * - every 10 minutes while the page is visible, and on becoming visible after
 *   that long;
 * - once more 31 s after a first mismatch, so the second read the policy needs
 *   does not wait 10 minutes. A read that teaches nothing (a 5xx, a captive
 *   portal, an image with no such file) doubles that wait, 30 s up to 10 min;
 * - never two within 5 s: the reads a flapping socket asks for fold into one.
 *
 * Decisions (`decideStaleBuildAction`): after every read, every 15 s while a new
 * build is confirmed, and at once when a remote-control viewer's phase moves
 * (`notifyRcReloadSurface`), because a redial's first wait is a quarter of a
 * second.
 *
 * The resume: a session already cut and waiting on its redial is handed over.
 * Its redial is stopped (`holdRedial`), the args it would have used are written
 * to sessionStorage with the time, the path and the signed-in person, and the
 * page reloads. A reload that does not happen within 20 s gives the redial back
 * and deletes the record. The next load takes that record out of storage before
 * anything renders, and the viewer claims it once it has its device and its
 * person (`claimRcResume`): once, under a minute old, on the first page that
 * load rendered, for the same person, device and organization. It resumes
 * through the redial's own ladder (`resumeAfterReload` in `useRemoteControl`),
 * so a device that is still restarting is retried as the redial would have,
 * and the device decides consent exactly as it would have for the redial.
 */
import { reactive, watch } from 'vue'
import type { Router } from 'vue-router'
import { pageBuildId, readServedBuild } from './buildId'
import { claimReload, reloadClaimable } from './staleBuild'
import {
  CONFIRM_GAP_MS,
  NEW_BUILD_RELOAD_KEY,
  NEW_BUILD_RELOAD_WINDOW_MS,
  NO_TRACK,
  SETTLE_MS,
  bannerHint,
  clearRcResumeIntent,
  decideStaleBuildAction,
  resumeArgsFor,
  takeRcResumeIntent,
  trackServedBuild,
  writeRcResumeIntent,
  type NewBuildHint,
  type RcReloadSurface,
  type RcResumeArgs,
  type RcResumeIntent,
  type RcSessionState,
  type ServedBuildTrack,
  type StaleBuildAction,
  type StaleBuildSituation,
} from './staleBuildPolicy'

/** How often a visible page asks. */
export const READ_EVERY_MS = 10 * 60_000
/** How often a page that knows of a new build looks for its quiet moment. */
export const TICK_MS = 15_000
/** Input this recent means the person's hands are on the page. */
export const BUSY_MS = 5_000
/** Hidden this long with no input: a draft or a dialog left there may go. */
export const HIDDEN_IDLE_MS = 30 * 60_000
/** A reload that has not taken the page away by now did not happen. */
export const RELOAD_GIVE_UP_MS = 20_000
/** Reads closer together than this are folded into one: a socket that flaps asks once. */
export const MIN_READ_GAP_MS = 5_000

const TEXT_INPUT_TYPES = new Set(['text', 'search', 'email', 'url', 'tel', 'password', 'number'])
/** The remote host's keyboard surfaces: what is typed there is forwarded, never kept. */
const RC_CAPTURE_SELECTOR = '[data-rc-capture]'
/** Anything a component marks as lost by a reload (a key shown once, say). */
const UNSAVED_SELECTOR = '[data-unsaved]'
const OPEN_DIALOG_SELECTOR = '.v-dialog.v-overlay--active'

export interface NewBuildBanner {
  visible: boolean
  hint: NewBuildHint
}

/** What AppLayout, which owns the socket and sees the call, tells the watch. */
export interface StaleBuildSources {
  /** The `/ws` store's status: `disconnected` | `connecting` | `connected`. */
  wsStatus: () => string
  /** In a call, or on the call page at all (it leaves and rejoins on a pod roll, and lingers after a call ends). */
  meetingLive: () => boolean
  /** An upload in flight anywhere on the page (`api.upload`). */
  uploading: () => boolean
}

export interface StaleBuildWatchDeps {
  /** This page's build; `null` turns the reading off (the dev server, a test, an older shell). */
  own: string | null
  document: Pick<Document, 'querySelector' | 'addEventListener' | 'visibilityState'>
  navigator?: Pick<Navigator, 'onLine'>
  location: Pick<Location, 'reload' | 'pathname'>
  /** sessionStorage, or `undefined` where reading it throws. */
  storage: () => Storage | undefined
  read?: () => Promise<string | null>
  /** Monotonic ms for the in-page clocks; wall ms for what crosses a reload. */
  monoNow?: () => number
  wallNow?: () => number
  banner?: NewBuildBanner
  log?: (...args: unknown[]) => void
}

export interface StaleBuildWatch {
  bind(src: StaleBuildSources): () => void
  registerSurface(surface: RcReloadSurface): () => void
  surfaceChanged(): void
  claimRcResume(here: { agentId: string; tenantId: string | null; path: string; userId: string | null }): RcResumeArgs | null
  navigationCompleted(): void
  reloadNow(): void
  dismiss(): void
  readNow(): Promise<void>
  evaluate(): void
  readonly track: ServedBuildTrack
}

function isTextEntry(el: EventTarget | null): el is HTMLElement {
  const e = el as HTMLElement | null
  if (!e || typeof e.closest !== 'function') return false
  if (e.closest(RC_CAPTURE_SELECTOR)) return false
  if (e.tagName === 'TEXTAREA') return true
  if (e.tagName === 'INPUT') return TEXT_INPUT_TYPES.has(((e as HTMLInputElement).type || 'text').toLowerCase())
  const ce = e.getAttribute('contenteditable')
  return e.isContentEditable === true || ce === '' || ce === 'true' || ce === 'plaintext-only'
}

function holdsText(el: HTMLElement): boolean {
  if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {
    return ((el as HTMLInputElement).value ?? '').trim() !== ''
  }
  return (el.textContent ?? '').trim() !== '' || el.querySelector('img, video, iframe') !== null
}

export function createStaleBuildWatch(deps: StaleBuildWatchDeps): StaleBuildWatch {
  const monoNow = deps.monoNow ?? (() => performance.now())
  const wallNow = deps.wallNow ?? (() => Date.now())
  const read = deps.read ?? (() => readServedBuild())
  const log = deps.log ?? ((...args: unknown[]) => console.info('[build]', ...args))
  const banner = deps.banner ?? newBuildBanner
  const doc = deps.document
  const own = deps.own

  function storage(): Storage | undefined {
    try {
      return deps.storage()
    } catch {
      return undefined
    }
  }

  // Out of storage before anything renders: the resume serves this load or none.
  let pendingResume: RcResumeIntent | null = takeRcResumeIntent(storage())
  let navigations = 0

  let track: ServedBuildTrack = NO_TRACK
  let reading = false
  let reloading = false
  /** The 10-minute clock's base: the page's load counts as a read. */
  let lastReadAt = monoNow()
  /** The last read actually sent, for the 5 s fold. */
  let lastAttemptAt = Number.NEGATIVE_INFINITY
  let confirmTimer: ReturnType<typeof setTimeout> | null = null
  let deferredRead: ReturnType<typeof setTimeout> | null = null
  /** Reads in a row that taught nothing: each one doubles the wait before the next confirming read. */
  let failedReads = 0
  /** A resume this page recorded for a reload that has not happened yet. */
  let resumeRecorded = false
  let lastAction: StaleBuildAction = 'none'
  let dismissedFor: string | null = null
  let sources: StaleBuildSources | null = null
  let lastWsChangeAt = monoNow()
  let lastInputAt = Number.NEGATIVE_INFINITY
  let hiddenSince: number | null = doc.visibilityState === 'hidden' ? monoNow() : null
  const surfaces = new Set<RcReloadSurface>()
  const typed = new Set<HTMLElement>()

  function onInput(e: Event): void {
    for (const el of typed) if (!el.isConnected) typed.delete(el)
    if (isTextEntry(e.target)) typed.add(e.target)
  }

  function onActivity(): void {
    lastInputAt = monoNow()
  }

  function onVisibility(): void {
    const now = monoNow()
    if (doc.visibilityState === 'hidden') {
      hiddenSince ??= now
      return
    }
    hiddenSince = null
    if (now - lastReadAt >= READ_EVERY_MS) requestRead()
    else evaluate()
  }

  function tick(): void {
    if (doc.visibilityState !== 'hidden' && monoNow() - lastReadAt >= READ_EVERY_MS) {
      requestRead()
      return
    }
    if (track.confirmed) evaluate()
  }

  function hasTypedText(): boolean {
    for (const el of typed) if (!el.isConnected || !holdsText(el)) typed.delete(el)
    return typed.size > 0
  }

  function settled(now: number): boolean {
    // A page with no socket (signed out) has no roll to wait out.
    if (!sources) return true
    let status: string
    try {
      status = sources.wsStatus()
    } catch {
      return false
    }
    // Connected, and for a while: a socket that is down, or retrying (a hidden
    // tab stretches its 3 s reconnect), is no sign the roll is over.
    return status === 'connected' && now - lastWsChangeAt >= SETTLE_MS
  }

  /** Read a source; one that throws counts as `fallback`, the side that never reloads. */
  function ask(get: ((s: StaleBuildSources) => boolean) | null, fallback: boolean): boolean {
    if (!sources || !get) return false
    try {
      return get(sources) === true
    } catch {
      return fallback
    }
  }

  function situation(): StaleBuildSituation {
    const now = monoNow()
    const states = [...surfaces].map((x) => x.state())
    const rc: RcSessionState = states.some((r) => r.session === 'live')
      ? 'live'
      : states.some((r) => r.session === 'redialing')
        ? 'redialing'
        : 'none'
    return {
      online: deps.navigator?.onLine !== false,
      confirmed: track.confirmed !== null,
      settled: settled(now),
      rc,
      rcExternal: states.some((r) => r.external && r.session !== 'none'),
      rcWork: states.some((r) => r.work),
      rcAsksConsent: states.some((r) => r.session !== 'none' && r.asksConsent),
      // Unknown is treated as live: the banner, never a reload out of a call.
      meetingLive: ask((s) => s.meetingLive(), true),
      held:
        doc.querySelector(UNSAVED_SELECTOR) !== null ||
        states.some((r) => r.hold) ||
        ask((s) => s.uploading(), true),
      unsavedInput: hasTypedText(),
      dialogOpen: doc.querySelector(OPEN_DIALOG_SELECTOR) !== null,
      hiddenIdle:
        hiddenSince !== null && now - hiddenSince >= HIDDEN_IDLE_MS && now - lastInputAt >= HIDDEN_IDLE_MS,
      busy: now - lastInputAt < BUSY_MS,
      reloadAllowed: reloadClaimable(storage(), wallNow(), NEW_BUILD_RELOAD_KEY, NEW_BUILD_RELOAD_WINDOW_MS),
    }
  }

  function showBanner(visible: boolean, hint: NewBuildHint): void {
    banner.visible = visible && track.confirmed !== null && dismissedFor !== track.confirmed
    banner.hint = hint
  }

  function startReload(held: RcReloadSurface | null): void {
    reloading = true
    banner.visible = false
    // A reload a page-leave prompt cancelled, or one that never started: the
    // redial goes on, and the page looks again (its claim is spent, so it
    // shows the banner rather than trying again). The resume recorded for it
    // goes too, or a later reload of this tab inside the minute would open
    // the session a second time beside the redial, and on a device that asks
    // its user, ask twice.
    setTimeout(() => {
      reloading = false
      if (resumeRecorded) {
        clearRcResumeIntent(storage())
        resumeRecorded = false
      }
      held?.releaseRedial()
      evaluate()
    }, RELOAD_GIVE_UP_MS)
    deps.location.reload()
  }

  /** Record the resume for the next load; false when it cannot be (no storage, nobody signed in). */
  function recordResume(surface: RcReloadSurface, args: RcResumeArgs): boolean {
    resumeRecorded = writeRcResumeIntent(storage(), args, wallNow(), deps.location.pathname, surface.userId())
    return resumeRecorded
  }

  function reloadQuiet(): void {
    if (!claimReload(storage(), wallNow(), NEW_BUILD_RELOAD_KEY, NEW_BUILD_RELOAD_WINDOW_MS)) {
      showBanner(true, null)
      return
    }
    log(`reloading: this page runs ${own}, the server serves ${track.confirmed}`)
    startReload(null)
  }

  function reloadAndResume(): void {
    const surface = [...surfaces].find((x) => x.state().session === 'redialing') ?? null
    const args = surface?.holdRedial() ?? null
    if (!surface || !args) {
      showBanner(true, 'rcResume')
      return
    }
    if (
      !claimReload(storage(), wallNow(), NEW_BUILD_RELOAD_KEY, NEW_BUILD_RELOAD_WINDOW_MS) ||
      !recordResume(surface, args)
    ) {
      surface.releaseRedial()
      showBanner(true, 'rcResume')
      return
    }
    log(`reloading in place of the redial: this page runs ${own}, the server serves ${track.confirmed}; the session resumes after the load`)
    startReload(surface)
  }

  function evaluate(): void {
    if (reloading) return
    if (!track.confirmed) {
      lastAction = 'none'
      showBanner(false, null)
      return
    }
    const s = situation()
    const action = decideStaleBuildAction(s)
    if (action !== lastAction) log(`new build ${track.confirmed}: ${action}`)
    lastAction = action
    if (action === 'reload') return reloadQuiet()
    if (action === 'reload-and-resume') return reloadAndResume()
    showBanner(action === 'banner', bannerHint(s))
  }

  /**
   * The confirming read: 31 s after the first mismatch, and never sooner than
   * the backoff after a read that taught nothing. Without that floor, a pending
   * mismatch whose 31 s had passed re-read at once after each failure, forever
   * where the server answers no file (a 5xx mid-roll, a captive portal, a
   * rolled-back image that serves the SPA shell for it).
   */
  function armConfirm(): void {
    if (confirmTimer !== null || !track.pending || track.confirmed) return
    const backoff = failedReads > 0 ? Math.min(READ_EVERY_MS, CONFIRM_GAP_MS * 2 ** (failedReads - 1)) : 0
    const wait = Math.max(track.pending.since + CONFIRM_GAP_MS + 1_000 - monoNow(), backoff, 0)
    confirmTimer = setTimeout(() => {
      confirmTimer = null
      void readNow()
    }, wait)
  }

  /** A read for an event (the socket, the clock, the tab shown): reads closer than `MIN_READ_GAP_MS` fold into one. */
  function requestRead(): void {
    if (own === null || reading || deferredRead !== null) return
    const since = monoNow() - lastAttemptAt
    if (since >= MIN_READ_GAP_MS) {
      void readNow()
      return
    }
    deferredRead = setTimeout(() => {
      deferredRead = null
      void readNow()
    }, MIN_READ_GAP_MS - since)
  }

  async function readNow(): Promise<void> {
    if (own === null || reading || reloading) return
    if (deps.navigator?.onLine === false) return
    reading = true
    lastReadAt = lastAttemptAt = monoNow()
    let served: string | null = null
    try {
      served = await read()
    } catch {
      served = null
    } finally {
      reading = false
    }
    failedReads = served === null ? failedReads + 1 : 0
    const before = track
    track = trackServedBuild(track, own, served, monoNow())
    if (track !== before) {
      if (track.confirmed && !before.confirmed) {
        log(`the server serves ${track.confirmed} (read twice, ${CONFIRM_GAP_MS / 1000} s apart); this page runs ${own}`)
      } else if (track.pending && track.pending !== before.pending) {
        log(`the server serves ${track.pending.build}, this page runs ${own}: reading again in ${CONFIRM_GAP_MS / 1000} s before acting`)
      } else if (track === NO_TRACK) {
        log(`the server serves this page's build again (${own})`)
      }
    }
    armConfirm()
    evaluate()
  }

  function bind(src: StaleBuildSources): () => void {
    sources = src
    lastWsChangeAt = monoNow()
    const stop = watch(src.wsStatus, (status) => {
      lastWsChangeAt = monoNow()
      if (status === 'connected') requestRead()
      else evaluate()
    })
    if (src.wsStatus() === 'connected') requestRead()
    return () => {
      stop()
      if (sources === src) sources = null
    }
  }

  function registerSurface(surface: RcReloadSurface): () => void {
    surfaces.add(surface)
    return () => {
      surfaces.delete(surface)
      evaluate()
    }
  }

  function claimRcResume(here: {
    agentId: string
    tenantId: string | null
    path: string
    userId: string | null
  }): RcResumeArgs | null {
    const intent = pendingResume
    pendingResume = null
    if (!intent) return null
    const args = resumeArgsFor(intent, { ...here, firstRoute: navigations === 1 }, wallNow())
    log(
      args
        ? 'resuming the remote session the reload for a new build interrupted'
        : 'a resume was recorded for another page, or has expired: not resuming',
    )
    return args
  }

  function reloadNow(): void {
    if (reloading) return
    let held: RcReloadSurface | null = null
    for (const surface of surfaces) {
      const st = surface.state()
      if (st.session === 'none' || st.external) continue
      const args = st.session === 'redialing' ? surface.holdRedial() : surface.resumeArgs()
      if (st.session === 'redialing' && args) held = surface
      // The person's own click: no claim (it cannot loop), and the session
      // resumes after the load as the banner said it would.
      if (args) recordResume(surface, args)
      break
    }
    startReload(held)
  }

  function dismiss(): void {
    dismissedFor = track.confirmed
    banner.visible = false
  }

  if (own !== null) {
    doc.addEventListener('input', onInput, true)
    for (const type of ['keydown', 'pointerdown', 'wheel', 'touchstart'] as const) {
      doc.addEventListener(type, onActivity, { capture: true, passive: true })
    }
    doc.addEventListener('visibilitychange', onVisibility)
    setInterval(tick, TICK_MS)
  }

  return {
    bind,
    registerSurface,
    surfaceChanged: () => evaluate(),
    claimRcResume,
    navigationCompleted: () => {
      navigations += 1
    },
    reloadNow,
    dismiss,
    readNow,
    evaluate,
    get track() {
      return track
    },
  }
}

// ── The page's one watch ───────────────────────────────────────────────────

/** What `components/layout/NewBuildBanner.vue` shows. */
export const newBuildBanner = reactive<NewBuildBanner>({ visible: false, hint: null })

let active: StaleBuildWatch | null = null

/**
 * Start the page's watch: in `main.ts`, before the first navigation, so the
 * resume a reload handed over is taken before any page can render.
 */
export function installStaleBuildWatch(router: Pick<Router, 'afterEach'>, w: Window = window): void {
  if (active) return
  active = createStaleBuildWatch({
    own: import.meta.env.DEV ? null : pageBuildId(w.document),
    document: w.document,
    navigator: w.navigator,
    location: w.location,
    storage: () => w.sessionStorage,
  })
  router.afterEach((_to, _from, failure) => {
    if (!failure) active?.navigationCompleted()
  })
}

/** AppLayout: the socket and the call. Returns the unbind for its unmount. */
export function bindStaleBuildWatch(src: StaleBuildSources): () => void {
  return active ? active.bind(src) : () => {}
}

/** The remote-control viewer, for its life. */
export function registerRcReloadSurface(surface: RcReloadSurface): () => void {
  return active ? active.registerSurface(surface) : () => {}
}

/** The viewer's phase moved: a redial may have just started. */
export function notifyRcReloadSurface(): void {
  active?.surfaceChanged()
}

/** The viewer, once it has its device: the session to resume, if this load is a reload that handed one over. */
export function claimRcResume(here: {
  agentId: string
  tenantId: string | null
  path: string
  userId: string | null
}): RcResumeArgs | null {
  return active ? active.claimRcResume(here) : null
}

/** The banner's Reload. */
export function reloadForNewBuild(): void {
  active?.reloadNow()
}

/** The banner's Later: hidden until a newer build is confirmed. */
export function dismissNewBuildBanner(): void {
  active?.dismiss()
}
