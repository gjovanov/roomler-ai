// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #3582 — which build this page runs, and which build the server serves now.
 *
 * Each build names itself ONCE (`ui/buildIdPlugin.ts`): a hash of the names of
 * the files it emits, every one of them content-hashed by Vite, so the id
 * changes whenever the bundle's code, styles or assets do (not for a file
 * copied from `ui/public/`, nor for an edit to index.html alone). That one value
 * lands in two places:
 *
 * - `<meta name="roomler-build">` in the page shell: what a tab carries for as
 *   long as it lives;
 * - `/version.json` beside the bundle: what the server serves NOW. nginx answers
 *   it from `location /` like any file in the bundle's root, with
 *   `Cache-Control: no-cache` and the security headers kept
 *   (`files/nginx-pod.conf`); the read below adds `no-store` and a query no
 *   cache has seen.
 *
 * Neither is the git commit. The UI stage of the image build has none (the
 * `GIT_SHA` build arg reaches only the runtime stage, and `.dockerignore` drops
 * `.git`), and an id that changed with every commit would reload every open tab
 * after a server-only promote and rename every chunk with it.
 */

export const VERSION_FILE = 'version.json'
export const VERSION_PATH = `/${VERSION_FILE}`
export const BUILD_META_NAME = 'roomler-build'

/** Hex, as the build writes it (16 today); a little room either way. */
const BUILD_ID = /^[0-9a-f]{8,64}$/

export function isBuildId(v: unknown): v is string {
  return typeof v === 'string' && BUILD_ID.test(v)
}

/**
 * The build this page runs, from its shell. `null` under the dev server, in a
 * unit test, and in a shell built before #3582: nothing to compare, so nothing
 * is ever reloaded for it.
 */
export function pageBuildId(doc: Pick<Document, 'querySelector'> | undefined = globalThis.document): string | null {
  try {
    const v = doc?.querySelector(`meta[name="${BUILD_META_NAME}"]`)?.getAttribute('content')
    return isBuildId(v) ? v : null
  } catch {
    return null
  }
}

/**
 * The id a `/version.json` body names; `null` for anything else. An image
 * older than #3582 has no such file, and nginx answers the SPA shell for it.
 */
export function parseVersionFile(body: unknown): string | null {
  if (!body || typeof body !== 'object') return null
  const build = (body as { build?: unknown }).build
  return isBuildId(build) ? build : null
}

export interface ReadServedBuildDeps {
  fetch?: (input: string, init: RequestInit) => Promise<Response>
  now?: () => number
  timeoutMs?: number
}

/**
 * Ask the server which build it serves now. Never throws: `null` means nothing
 * was learned (offline, a failed or slow request, a body that is not the file).
 */
export async function readServedBuild(deps: ReadServedBuildDeps = {}): Promise<string | null> {
  const url = `${VERSION_PATH}?t=${(deps.now ?? Date.now)()}`
  const ctrl = typeof AbortController === 'function' ? new AbortController() : null
  const timer = ctrl ? setTimeout(() => ctrl.abort(), deps.timeoutMs ?? 10_000) : null
  const init: RequestInit = {
    cache: 'no-store',
    credentials: 'same-origin',
    headers: { Accept: 'application/json' },
    ...(ctrl ? { signal: ctrl.signal } : {}),
  }
  try {
    // The global is called as itself: a copied reference with a foreign
    // `this` throws "Illegal invocation" in some engines.
    const res = deps.fetch
      ? await deps.fetch(url, init)
      : typeof globalThis.fetch === 'function'
        ? await globalThis.fetch(url, init)
        : null
    if (!res || !res.ok) return null
    // The SPA shell nginx answers a missing file with is `text/html`.
    if (!(res.headers.get('content-type') ?? '').toLowerCase().includes('json')) return null
    return parseVersionFile(await res.json())
  } catch {
    return null
  } finally {
    if (timer !== null) clearTimeout(timer)
  }
}
