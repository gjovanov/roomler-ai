// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * A tab that outlived a promote. Every image ships a new set of
 * content-hashed chunks and none of the old ones, so a tab still running the
 * previous build asks for a route's chunk that no longer exists: the import
 * 404s, the navigation is dropped, and the click does nothing at all.
 *
 * The cure is the page the server serves now, so: load it once. Vite reports
 * a failed dynamic import as `vite:preloadError` (its documented hook for
 * exactly this), and the router reports which navigation it cost, so the
 * reload lands where the person was going rather than where they were.
 *
 * Once, never a loop: a reload is recorded in sessionStorage, and a second
 * failure inside `RELOAD_WINDOW_MS` leaves the error alone, because a chunk
 * missing from the build being served is not cured by asking again. Where
 * storage cannot be written there is no record to stop a loop with, so
 * nothing reloads at all: the old behaviour, never a worse one.
 *
 * This catches only a tab that navigates. A tab that stays on one page (a
 * remote-control viewer left open all day, #3582) never asks for a chunk, so
 * nothing here ever fires for it. `utils/staleBuildWatch.ts` covers that tab:
 * it compares the build the page runs with the one the server serves
 * (`/version.json`) and reloads at a moment that loses nothing, through the
 * same claim, under its own key and a 10-minute window.
 */
import type { Router } from 'vue-router'

export const RELOAD_KEY = 'roomler.staleBuildReloadAt'
export const RELOAD_WINDOW_MS = 10_000

/** The messages a failed dynamic import carries in Chrome, Firefox and Safari. */
const CHUNK_ERROR =
  /Failed to fetch dynamically imported module|error loading dynamically imported module|Importing a module script failed|Unable to preload CSS/i

export function isChunkLoadError(err: unknown): boolean {
  const message = err instanceof Error ? err.message : String(err ?? '')
  return CHUNK_ERROR.test(message)
}

/**
 * Records the reload and returns true when one may happen now; false inside
 * the window of the last one, or when storage cannot hold the record.
 * `key` and `windowMs` let another reloader keep its own record
 * (`staleBuildPolicy.ts`'s `NEW_BUILD_RELOAD_KEY`).
 */
export function claimReload(
  storage: Pick<Storage, 'getItem' | 'setItem'> | undefined,
  now: number,
  key: string = RELOAD_KEY,
  windowMs: number = RELOAD_WINDOW_MS,
): boolean {
  if (!storage) return false
  try {
    const last = Number(storage.getItem(key) ?? 0)
    if (Number.isFinite(last) && now - last < windowMs) return false
    storage.setItem(key, String(now))
    return true
  } catch {
    return false
  }
}

/**
 * Whether `claimReload` could claim now, without recording anything: false
 * inside the window, or when storage cannot be read. (Whether it can be
 * WRITTEN is learned only by the claim itself, which then refuses.)
 */
export function reloadClaimable(
  storage: Pick<Storage, 'getItem'> | undefined,
  now: number,
  key: string = RELOAD_KEY,
  windowMs: number = RELOAD_WINDOW_MS,
): boolean {
  if (!storage) return false
  try {
    const last = Number(storage.getItem(key) ?? 0)
    return !(Number.isFinite(last) && now - last < windowMs)
  } catch {
    return false
  }
}

interface Host {
  addEventListener: Window['addEventListener']
  sessionStorage?: Storage
  location: Pick<Location, 'reload' | 'assign'>
  navigator?: Pick<Navigator, 'onLine'>
  setTimeout: (fn: () => void, ms: number) => unknown
}

export function installStaleBuildRecovery(host: Host, router: Pick<Router, 'onError'>): void {
  // The navigation a failed chunk cost, if the router claims it. Vite's event
  // fires first, inside the import, and the router's rejection lands a
  // microtask later, so the recovery waits one macrotask to learn the target.
  // Either signal alone starts it; both together start it once.
  let target: string | null = null
  let pending = false

  const recover = () => {
    pending = false
    const to = target
    target = null
    // Offline, the reload would trade the app for the browser's error page,
    // and the chunk is not gone, only out of reach.
    if (host.navigator?.onLine === false) return
    let storage: Storage | undefined
    try {
      storage = host.sessionStorage
    } catch {
      storage = undefined
    }
    if (!claimReload(storage, Date.now())) return
    if (to) host.location.assign(to)
    else host.location.reload()
  }
  const schedule = () => {
    if (pending) return
    pending = true
    host.setTimeout(recover, 0)
  }

  router.onError((err, to) => {
    if (!isChunkLoadError(err)) return
    if (to) target = to.fullPath
    schedule()
  })

  // Never `preventDefault()`ed: Vite would then swallow the error, and the
  // router, which names the target, would never hear of it.
  host.addEventListener('vite:preloadError', schedule)
}
