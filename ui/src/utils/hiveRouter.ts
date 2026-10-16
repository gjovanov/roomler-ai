// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-90 — one router for the `hive:*` frames of every peer this tab holds: a
 * session's viewer (`useHiveViewer`) and a search (`useHiveSearch`, P2i-3).
 *
 * ⚠️ The ws store holds ONE handler per frame type (`onMediaMessage` is a
 * `Map.set`). A second composable that registered `hive:view.answer` itself
 * would silently take every viewer's answers away, so no peer registers a
 * frame type: each asks here, its opening answer by the `ref` it asked with,
 * the rest of its handshake by its grant. A search's peer takes the view's
 * frames by its grant (P2i-2), so the two share every type but their first.
 */
import type { useWsStore } from '@/stores/ws'

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type HiveFrame = { type: string } & Record<string, any>
type Handler = (f: HiveFrame) => void

const byRef = new Map<string, Handler>()
const byGrant = new Map<string, Handler>()

/** An open's answer, by the `ref` it was asked with. */
const BY_REF = ['hive:view.ready', 'hive:view.refused', 'hive:search.ready', 'hive:search.refused']
/** The rest of a peer's handshake, by its grant: a view's and a search's alike. */
const BY_GRANT = ['hive:view.answer', 'hive:view.ice', 'hive:view.renewed', 'hive:view.closed']

/** Register the router's handlers. Called at every open: `disconnect()`
 *  clears the store's handlers. */
export function routeHiveFrames(ws: ReturnType<typeof useWsStore>): void {
  for (const type of BY_REF) {
    ws.onMediaMessage(type, (data) => {
      const handler = data?.ref ? byRef.get(data.ref) : undefined
      handler?.({ type, ...data })
    })
  }
  for (const type of BY_GRANT) {
    ws.onMediaMessage(type, (data) => {
      const handler = data?.grant_id ? byGrant.get(data.grant_id) : undefined
      handler?.({ type, ...data })
    })
  }
}

export function onRef(ref: string, handler: Handler): void {
  byRef.set(ref, handler)
}

export function offRef(ref: string): void {
  byRef.delete(ref)
}

export function onGrant(grantId: string, handler: Handler): void {
  byGrant.set(grantId, handler)
}

export function offGrant(grantId: string): void {
  byGrant.delete(grantId)
}

/** A fresh `ref` for an open. */
export function newRef(): string {
  return Math.random().toString(36).slice(2, 12)
}
