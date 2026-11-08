// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #1637 — a call switch that crosses a remount.
 *
 * The call page is keyed by its room (`meta.remountOn: 'roomId'`), so the page
 * that leaves room A's call is not the page that joins room B's: A's page asks,
 * leaves, and hands the join to the page the router mounts next, once.
 *
 * A module variable, not a store, not the URL and not `history.state`: a join
 * that survived a reload, or a back-and-forward, would put a camera into a call
 * nobody just asked to join. Single-use, for that one room, and only for a few
 * seconds, so a navigation that never arrived cannot join a call later.
 */

/** Long enough for the router to mount the next page; far too short to outlive the click. */
export const CALL_SWITCH_HANDOFF_MS = 10_000

let pending: { roomId: string; at: number } | null = null

/** A's page, having left its call: the page for `roomId` joins its call when it mounts. */
export function handOffJoin(roomId: string, now: number = Date.now()): void {
  pending = { roomId, at: now }
}

/** The page for `roomId`, on mount: was it asked to join? Clears the hand-off either way. */
export function takeJoin(roomId: string, now: number = Date.now()): boolean {
  const p = pending
  pending = null
  return !!p && p.roomId === roomId && now - p.at <= CALL_SWITCH_HANDOFF_MS
}
