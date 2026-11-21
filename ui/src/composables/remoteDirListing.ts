// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * The drawer's folder listings on the files channel (#4121): the request, its
 * wait, and the two answers that settle it, `files:dir-list` and
 * `files:dir-error`, matched by `req_id`. Moved out of `useRemoteControl.ts`;
 * the composable hands in its channels, and a test its timers
 * (`remoteDirListing.spec.ts`).
 *
 * A listing that fits in one message comes whole, as from every agent. One
 * that does not comes in pages when the request asked for them (`paged`):
 * each a `files:dir-list` of its own, numbered `page` (from 0) of `pages`,
 * with the listing's `total`, and the last one `capped` when the folder held
 * more than the agent lists. An agent that predates pages answers a request
 * with `paged` as before, and one that pages answers a viewer that does not
 * ask with a `files:dir-error` naming the folder.
 */

export type DirEntry = {
  name: string
  is_dir: boolean
  size: number | null
  mtime_unix: number | null
}
export type DirListing = {
  path: string
  parent: string | null
  entries: DirEntry[]
  /** The folder held more entries than the agent lists (10 000). */
  capped?: boolean
}

/** What the files channel's demux reads from a message it hands here. */
export type DirAnswerMsg = {
  t?: string
  req_id?: string
  path?: string
  parent?: string | null
  entries?: DirEntry[]
  message?: string
  /** A page of a listing sent in pages: this one, from 0, of `pages`. */
  page?: number
  pages?: number
  /** Every page's entries together. */
  total?: number
  capped?: boolean
}

/** The part of a data channel a listing uses. */
export type DirListingChannel = {
  readonly readyState: string
  send(data: string): void
}

type Timer = ReturnType<typeof globalThis.setTimeout>

export interface DirListingDeps {
  /** The composable's channels; a listing goes out on `files`. */
  channels: { readonly files?: DirListingChannel | null }
  /** Whether the host's agent sends a large listing in pages
   *  ([`agentSendsDirPages`]); without it, a timeout says an update lists one. */
  hasDirPages?: () => boolean
  setTimeout?: (fn: () => void, ms: number) => Timer
  clearTimeout?: (timer: Timer) => void
}

/** How long a listing waits for its answer, and for each page after one. */
const LISTING_WAIT_MS = 5_000

/** Whether the host's agent sends a listing too large for one message in
 *  pages: `dir-pages` in its hello's `files`, matched by equality. */
export function agentSendsDirPages(
  agent: { capabilities?: { files?: readonly string[] } | null } | null | undefined,
): boolean {
  const files = agent?.capabilities?.files
  return Array.isArray(files) && files.includes('dir-pages')
}

/** The folder a request names, for the words about it; the roots view
 *  (`isRootsRequest`) has none. */
function folderOf(path: string): string | null {
  return path === '' || path === '~' ? null : path
}

export function createDirListing(deps: DirListingDeps) {
  const { channels } = deps
  const hasDirPages = deps.hasDirPages ?? (() => false)
  const setTimeout = deps.setTimeout ?? ((fn: () => void, ms: number) => globalThis.setTimeout(fn, ms))
  const clearTimeout = deps.clearTimeout ?? ((timer: Timer) => globalThis.clearTimeout(timer))

  // --------------------------------------------------------------
  // Directory listing (Phase 3 of file-DC v2).
  //
  // Request/response keyed by req_id, like the clipboard:read flow.
  // A 5 s wait, from the request and again from each page, rejects a
  // listing nothing answers, so the drawer doesn't spin forever if the
  // host is unreachable or its agent could not send the answer.

  type Pending = {
    /** The path asked for, for the words about it. */
    path: string
    resolve: (l: DirListing) => void
    reject: (e: Error) => void
    timer: Timer
    /** A listing in pages, as far as it has come. */
    joined?: { pages: number; total: number; next: number; listing: DirListing }
  }
  const pendingDirRequests = new Map<string, Pending>()
  function settleDirRequest(reqId: string): {
    resolve: (l: DirListing) => void
    reject: (e: Error) => void
  } | null {
    const p = pendingDirRequests.get(reqId)
    if (!p) return null
    clearTimeout(p.timer)
    pendingDirRequests.delete(reqId)
    return { resolve: p.resolve, reject: p.reject }
  }

  /** The wait for `reqId`'s next frame. */
  function armWait(reqId: string): Timer {
    return setTimeout(() => {
      const p = pendingDirRequests.get(reqId)
      if (!p) return
      pendingDirRequests.delete(reqId)
      p.reject(new Error(timeoutWords(p)))
    }, LISTING_WAIT_MS)
  }

  /** What a listing that waited 5 s in vain says. Never that the host may
   *  not support browsing: the drawer asks only an agent that said it does. */
  function timeoutWords(p: Pending): string {
    const folder = folderOf(p.path)
    if (p.joined) {
      const { listing, total } = p.joined
      return `The listing of ${folder ?? listing.path} stopped arriving (${listing.entries.length} of ${total} entries). Try again.`
    }
    if (folder === null) return 'No answer from the host within 5 s.'
    const words = `No answer from the host for ${folder} within 5 s.`
    // An agent that does not page cannot send a listing over 64 KiB at all.
    return hasDirPages() ? words : `${words} To list a large folder, the host's agent needs an update.`
  }

  /** List a directory on the host. `path` is the absolute host path;
   *  the empty string / "~" enumerate the roots (logical drives on
   *  Windows; a single "/" on Unix) — see `isRootsRequest`. "/" itself
   *  is a directory and lists the Unix root (#2870). Resolves with the
   *  listing or rejects on timeout / dir-error. */
  function listDir(path: string): Promise<DirListing> {
    const ch = channels.files
    if (!ch || ch.readyState !== 'open') {
      return Promise.reject(new Error('files channel not open'))
    }
    const reqId = `ls-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`
    return new Promise<DirListing>((resolve, reject) => {
      const timer = armWait(reqId)
      pendingDirRequests.set(reqId, { path, resolve, reject, timer })
      try {
        ch.send(JSON.stringify({ t: 'files:dir', req_id: reqId, path, paged: true }))
      } catch (e) {
        clearTimeout(timer)
        pendingDirRequests.delete(reqId)
        reject(e instanceof Error ? e : new Error(String(e)))
      }
    })
  }

  /** One page of `reqId`'s listing: joined in order, the wait re-armed, and
   *  the listing resolved at its last page. A page out of order, or a last
   *  page that leaves the join short of `total`, rejects naming the folder. */
  function takePage(reqId: string, p: Pending, msg: DirAnswerMsg): void {
    const { page, pages, total } = msg
    const numbered =
      Number.isInteger(page) &&
      Number.isInteger(pages) &&
      Number.isInteger(total) &&
      (pages as number) >= 1 &&
      (total as number) >= 0
    const joined = (p.joined ??= {
      pages: pages as number,
      total: total as number,
      next: 0,
      listing: {
        path: String(msg.path ?? ''),
        parent: typeof msg.parent === 'string' ? msg.parent : null,
        entries: [],
      },
    })
    const entries = Array.isArray(msg.entries) ? msg.entries : []
    const folder = folderOf(p.path) ?? joined.listing.path
    if (!numbered || page !== joined.next || pages !== joined.pages || total !== joined.total) {
      settleDirRequest(reqId)?.reject(new Error(`The listing of ${folder} arrived incomplete. Try again.`))
      return
    }
    for (const entry of entries) joined.listing.entries.push(entry)
    joined.next += 1
    if (joined.next < joined.pages) {
      clearTimeout(p.timer)
      p.timer = armWait(reqId)
      return
    }
    const got = joined.listing.entries.length
    if (got !== joined.total) {
      settleDirRequest(reqId)?.reject(
        new Error(`The listing of ${folder} arrived incomplete (${got} of ${joined.total} entries). Try again.`),
      )
      return
    }
    joined.listing.capped = msg.capped === true
    settleDirRequest(reqId)?.resolve(joined.listing)
  }

  /** The files channel's demux, for an answer to a listing: true when `msg`
   *  was one, and the demux goes no further with it. */
  function onFilesMessage(msg: DirAnswerMsg): boolean {
    // Directory listing replies are demuxed by req_id, not id.
    if (msg.t === 'files:dir-list') {
      const reqId = typeof msg.req_id === 'string' ? msg.req_id : ''
      const p = pendingDirRequests.get(reqId)
      if (!p) return true
      if (msg.pages !== undefined) {
        takePage(reqId, p, msg)
        return true
      }
      // A whole listing, as every agent sends one that fits.
      settleDirRequest(reqId)?.resolve({
        path: String(msg.path ?? ''),
        parent: typeof msg.parent === 'string' ? msg.parent : null,
        entries: Array.isArray(msg.entries) ? msg.entries : [],
        capped: msg.capped === true,
      })
      return true
    } else if (msg.t === 'files:dir-error') {
      const reqId = typeof msg.req_id === 'string' ? msg.req_id : ''
      const pending = settleDirRequest(reqId)
      if (pending) {
        pending.reject(new Error(String(msg.message ?? 'agent dir error')))
      }
      return true
    }
    return false
  }

  return { listDir, onFilesMessage }
}

/** The drawer's navigations (#4121): `begin()` at each one, whose answer
 *  changes the drawer while the check it returns says so: only the latest
 *  navigation's, so a folder answered after a later one is dropped. */
export function createLatest(): { begin(): () => boolean } {
  let latest = 0
  return {
    begin() {
      const mine = ++latest
      return () => mine === latest
    },
  }
}
