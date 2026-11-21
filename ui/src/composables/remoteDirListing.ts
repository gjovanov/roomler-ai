// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * The drawer's folder listings on the files channel (#4121): the request, its
 * wait, and the two answers that settle it, `files:dir-list` and
 * `files:dir-error`, matched by `req_id`. Moved out of `useRemoteControl.ts`;
 * the composable hands in its channels, and a test its timers
 * (`remoteDirListing.spec.ts`).
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
}

/** What the files channel's demux reads from a message it hands here. */
export type DirAnswerMsg = {
  t?: string
  req_id?: string
  path?: string
  parent?: string | null
  entries?: DirEntry[]
  message?: string
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
  setTimeout?: (fn: () => void, ms: number) => Timer
  clearTimeout?: (timer: Timer) => void
}

export function createDirListing(deps: DirListingDeps) {
  const { channels } = deps
  const setTimeout = deps.setTimeout ?? ((fn: () => void, ms: number) => globalThis.setTimeout(fn, ms))
  const clearTimeout = deps.clearTimeout ?? ((timer: Timer) => globalThis.clearTimeout(timer))

  // --------------------------------------------------------------
  // Directory listing (Phase 3 of file-DC v2).
  //
  // Request/response keyed by req_id, like the clipboard:read flow.
  // 5 s timeout rejects stale requests so the drawer doesn't spin
  // forever if the host is unreachable or the agent lacks the new
  // capability (old 0.2.x agents will simply not reply).

  const pendingDirRequests = new Map<
    string,
    { resolve: (l: DirListing) => void; reject: (e: Error) => void; timer: ReturnType<typeof setTimeout> }
  >()
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
      const timer = setTimeout(() => {
        if (pendingDirRequests.has(reqId)) {
          pendingDirRequests.delete(reqId)
          reject(new Error('list_dir timed out (5 s) — host may not support remote browse'))
        }
      }, 5_000)
      pendingDirRequests.set(reqId, { resolve, reject, timer })
      try {
        ch.send(JSON.stringify({ t: 'files:dir', req_id: reqId, path }))
      } catch (e) {
        clearTimeout(timer)
        pendingDirRequests.delete(reqId)
        reject(e instanceof Error ? e : new Error(String(e)))
      }
    })
  }

  /** The files channel's demux, for an answer to a listing: true when `msg`
   *  was one, and the demux goes no further with it. */
  function onFilesMessage(msg: DirAnswerMsg): boolean {
    // Directory listing replies are demuxed by req_id, not id.
    if (msg.t === 'files:dir-list') {
      const reqId = typeof msg.req_id === 'string' ? msg.req_id : ''
      const pending = settleDirRequest(reqId)
      if (pending) {
        pending.resolve({
          path: String(msg.path ?? ''),
          parent: typeof msg.parent === 'string' ? msg.parent : null,
          entries: Array.isArray(msg.entries) ? msg.entries : [],
        })
      }
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
 *  changes the drawer while the check it returns says so. Every navigation's
 *  answer applies, so a folder answered after a later one replaces it. */
export function createLatest(): { begin(): () => boolean } {
  return {
    begin() {
      return () => true
    },
  }
}
