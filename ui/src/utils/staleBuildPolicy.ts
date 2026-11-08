// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #3582 — a tab that outlives a promote, as rules. Pure: what the page has read
 * about the build the server serves, what it decides to do about it, and the
 * resume a reload hands to the next page load. The wiring (when to read, what is
 * live on the page, the reload itself) is `utils/staleBuildWatch.ts`; the design
 * is docs/ui.md, "A tab that outlives a promote".
 *
 * | The page, once a new build is confirmed            | `decideStaleBuildAction` |
 * |----------------------------------------------------|--------------------------|
 * | nothing live, nothing typed, no dialog             | `reload`                 |
 * | a remote session cut and waiting on its redial     | `reload-and-resume`      |
 * | a live remote session, an external one (FR-52), a  | `banner`, and the reload |
 * | call, typed input, an open dialog                  | waits for a quiet moment |
 *
 * Four guards, each one a way the page could be left worse than today:
 * - **Two reads, 30 s apart** (`trackServedBuild`): during a roll one pod
 *   serves the old build and one the new, so a single read proves nothing.
 * - **A settled socket** (`settled`): until this page's own `/ws` has been
 *   steady for 30 s the server may be mid-roll, and a reload then can take its
 *   shell from one build and its chunks from the other: a blank page.
 * - **One automatic reload in 10 minutes**, claimed in sessionStorage
 *   (`utils/staleBuild.ts`); where storage cannot be written, none at all.
 * - **Offline, nothing.**
 *
 * Why the roll's own cut is not the moment, though the triage hoped it would
 * be: that cut is when the socket is not settled, and the page's redial has
 * the session back within seconds, long before a second read 30 s later could
 * confirm anything. So a session the roll cut comes back as it always did,
 * the page then shows the banner, and the reload with its resume happens at
 * the session's next cut on a settled socket (the device restarting for an
 * update, a network change on either end), at its end, or at the person's
 * click on Reload.
 */

/** A mismatch counts once the same id is read again at least this much later. */
export const CONFIRM_GAP_MS = 30_000
/** The page's own socket has been steady this long: no roll is cutting it. */
export const SETTLE_MS = 30_000
/** The automatic reload's own record (the chunk-failure one is `staleBuild.ts`'s). */
export const NEW_BUILD_RELOAD_KEY = 'roomler.newBuildReloadAt'
export const NEW_BUILD_RELOAD_WINDOW_MS = 10 * 60_000
/** The resume a reload hands to the next page load. */
export const RC_RESUME_KEY = 'roomler.rcResumeIntent'
export const RC_RESUME_MAX_AGE_MS = 60_000

// ── What the page has read ─────────────────────────────────────────────────

export interface ServedBuildTrack {
  /** An id the server served that is not this page's, and when it was first read (monotonic ms). */
  pending: { build: string; since: number } | null
  /** `pending.build`, once a read at least `CONFIRM_GAP_MS` after `since` named it again. */
  confirmed: string | null
}

export const NO_TRACK: ServedBuildTrack = Object.freeze({ pending: null, confirmed: null })

/**
 * Fold one read of the served build into what the page knows. `served` is
 * `null` when the read taught nothing (offline, a failed request, an older
 * image answering with the SPA shell), and that changes nothing.
 */
export function trackServedBuild(
  track: ServedBuildTrack,
  own: string,
  served: string | null,
  now: number,
): ServedBuildTrack {
  if (served === null) return track
  // The server serves this page's build, again after a rollback too.
  if (served === own) return NO_TRACK
  if (track.pending && track.pending.build === served) {
    if (track.confirmed === served || now - track.pending.since < CONFIRM_GAP_MS) return track
    return { pending: track.pending, confirmed: served }
  }
  // A first mismatch, or another id than the last one: a roll may be in
  // progress, so the count starts over.
  return { pending: { build: served, since: now }, confirmed: null }
}

// ── What is live on the page ───────────────────────────────────────────────

/**
 * A remote-control viewer, as the policy sees it (`rcReloadState` in
 * `composables/useRemoteControl.ts`):
 * - `none`: no session (idle, ended, failed);
 * - `redialing`: the session was cut and the page's ladder is waiting to open
 *   the next one, with nothing in flight;
 * - `live`: anything else, a request or a consent prompt in flight included.
 */
export type RcSessionState = 'none' | 'live' | 'redialing'

export interface RcReloadState {
  session: RcSessionState
  /** FR-52: an external session cannot come back without the device's password. */
  external: boolean
  /** Work the redial would carry on and a reload would drop: a file transfer, a recording's download. */
  work: boolean
}

/** What a viewer offers the watch; registered by `views/remote/RemoteControl.vue`. */
export interface RcReloadSurface {
  state(): RcReloadState
  /** What the session's redial uses; `null` for an external session or none. */
  resumeArgs(): RcResumeArgs | null
  /** Stop the pending redial for a reload and say what it would have used; `null` = not redialing, nothing touched. */
  holdRedial(): RcResumeArgs | null
  /** Give back a redial `holdRedial` stopped, when the reload did not happen. */
  releaseRedial(): void
}

export interface StaleBuildSituation {
  /** `navigator.onLine`: offline, a reload trades the app for the browser's error page. */
  online: boolean
  /** The served build differs from this page's, read the same twice `CONFIRM_GAP_MS` apart. */
  confirmed: boolean
  /** This page's own `/ws` has been steady `SETTLE_MS`, or the page has none. */
  settled: boolean
  rc: RcSessionState
  rcExternal: boolean
  rcWork: boolean
  /** In a call (the conference store's `isInCall`), on any page. */
  meetingLive: boolean
  /** Text the person typed and has not sent or saved, or an element marked `data-unsaved`. */
  unsavedInput: boolean
  /** A Vuetify dialog is open. */
  dialogOpen: boolean
  /** Hidden for a long while, with no input: a draft or a dialog left there may go. */
  hiddenIdle: boolean
  /** The person used a key, a pointer or the wheel a moment ago. */
  busy: boolean
  /** An automatic reload may be claimed: storage readable, none in the last 10 minutes. */
  reloadAllowed: boolean
}

export type StaleBuildAction = 'none' | 'reload' | 'reload-and-resume' | 'banner'

/**
 * What to do about a new build. `banner` is the answer whenever a reload would
 * lose something; the watch asks again as the page changes, so the reload
 * happens by itself at the next quiet moment.
 */
export function decideStaleBuildAction(s: StaleBuildSituation): StaleBuildAction {
  if (!s.online || !s.confirmed) return 'none'
  // What a reload would end and cannot bring back by itself. The issue's own
  // constraint: never in the middle of a remote session or a call.
  if (s.rcExternal && s.rc !== 'none') return 'banner'
  if (s.meetingLive) return 'banner'
  if (s.rc === 'live') return 'banner'
  if (s.rc === 'redialing') {
    // The session is already cut and its redial would open the next one: a
    // reload that resumes it costs the person about the page's load time, and
    // the host no consent it would not have asked for the redial anyway.
    if (s.rcWork || s.unsavedInput || s.dialogOpen) return 'banner'
    if (!s.settled || !s.reloadAllowed) return 'banner'
    return 'reload-and-resume'
  }
  if ((s.unsavedInput || s.dialogOpen) && !s.hiddenIdle) return 'banner'
  if (!s.settled || !s.reloadAllowed || s.busy) return 'banner'
  return 'reload'
}

/** The line under the banner's title: what Reload does to what is live. */
export type NewBuildHint = 'rcResume' | 'rcExternal' | 'call' | null

export function bannerHint(s: Pick<StaleBuildSituation, 'rc' | 'rcExternal' | 'meetingLive'>): NewBuildHint {
  if (s.rc !== 'none' && s.rcExternal) return 'rcExternal'
  if (s.meetingLive) return 'call'
  if (s.rc !== 'none') return 'rcResume'
  return null
}

// ── The resume a reload hands over ─────────────────────────────────────────

/** `useRemoteControl`'s `lastConnectArgs`: what its redial passes to `connect`. */
export interface RcResumeArgs {
  agentId: string
  permissions: string
  orgId?: string
}

export interface RcResumeIntent extends RcResumeArgs {
  v: 1
  /** Wall-clock ms when the reload was decided. */
  at: number
  /** `location.pathname` of the page that reloaded: the next load must be that page. */
  path: string
}

type IntentStorage = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>

const OBJECT_ID = /^[0-9a-f]{24}$/
/** `'VIEW | INPUT | CLIPBOARD | FILES | RECORD'`, the bitflags wire spelling. */
const PERMISSIONS = /^[A-Z_]{1,32}(?: \| [A-Z_]{1,32}){0,15}$/

function validArgs(a: unknown): a is RcResumeArgs {
  if (!a || typeof a !== 'object') return false
  const { agentId, permissions, orgId } = a as Record<string, unknown>
  return (
    typeof agentId === 'string' &&
    OBJECT_ID.test(agentId) &&
    typeof permissions === 'string' &&
    PERMISSIONS.test(permissions) &&
    (orgId === undefined || (typeof orgId === 'string' && OBJECT_ID.test(orgId)))
  )
}

function argsOf(a: RcResumeArgs): RcResumeArgs {
  return a.orgId ? { agentId: a.agentId, permissions: a.permissions, orgId: a.orgId } : { agentId: a.agentId, permissions: a.permissions }
}

/** Record the resume for the next page load; false when storage cannot hold it (then nothing reloads). */
export function writeRcResumeIntent(
  storage: IntentStorage | undefined,
  args: RcResumeArgs,
  now: number,
  path: string,
): boolean {
  if (!storage || !validArgs(args) || typeof path !== 'string' || !path.startsWith('/')) return false
  const intent: RcResumeIntent = { v: 1, ...argsOf(args), at: now, path }
  try {
    storage.setItem(RC_RESUME_KEY, JSON.stringify(intent))
    return true
  } catch {
    return false
  }
}

/**
 * Take the intent out of storage, used or not, so it can serve one page load
 * at most; the well-formed intent, else `null`.
 */
export function takeRcResumeIntent(storage: IntentStorage | undefined): RcResumeIntent | null {
  if (!storage) return null
  let raw: string | null
  try {
    raw = storage.getItem(RC_RESUME_KEY)
  } catch {
    return null
  }
  if (raw === null) return null
  try {
    storage.removeItem(RC_RESUME_KEY)
  } catch {
    /* a key that cannot be removed is still read only once: the watch keeps no copy */
  }
  try {
    const v: unknown = JSON.parse(raw)
    if (!v || typeof v !== 'object') return null
    const { v: version, at, path } = v as Record<string, unknown>
    if (version !== 1 || typeof at !== 'number' || !Number.isFinite(at)) return null
    if (typeof path !== 'string' || !path.startsWith('/')) return null
    if (!validArgs(v)) return null
    return { v: 1, ...argsOf(v), at, path }
  } catch {
    return null
  }
}

/**
 * The args the page now rendering may resume with: the intent is under a
 * minute old, this is the first page the load rendered (a sign-in in between
 * means the person is starting over), and it is the same device's viewer in
 * the same organization at the same path.
 */
export function resumeArgsFor(
  intent: RcResumeIntent | null,
  here: { agentId: string; tenantId: string | null; path: string; firstRoute: boolean },
  now: number,
): RcResumeArgs | null {
  if (!intent || !here.firstRoute) return null
  const age = now - intent.at
  // A small negative age is a clock step across the reload; a large one is not this reload.
  if (!(age > -5_000 && age < RC_RESUME_MAX_AGE_MS)) return null
  if (intent.agentId !== here.agentId || intent.path !== here.path) return null
  if (intent.orgId !== undefined && intent.orgId !== here.tenantId) return null
  return argsOf(intent)
}
