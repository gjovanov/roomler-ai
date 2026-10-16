// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-90 P2j-1 — reading a session's copies: which member runs it, how far
 * behind the primary each copy is, and which one a move should go to.
 *
 * Every number is the server's (`holds`, P2e-6a's rule: how much of the
 * session's chain a copy holds, a tail past where its fence ended counting
 * for nothing). Nothing here re-derives it from a tip, whose `(fence, seq)`
 * compared whole would read an empty copy at a newer fence as the freshest.
 */
import type { HiveReplicaMember, HiveSession } from '@/stores/hive'

/** The member that runs the session: its `primary` row. */
export function primaryOf(s: HiveSession): HiveReplicaMember | undefined {
  return s.replicaset?.members.find((m) => m.role === 'primary')
}

/** How many events a copy is behind the primary's, by what each holds;
 *  `null` when either has not said. */
export function behind(m: HiveReplicaMember, primary: HiveReplicaMember | undefined): number | null {
  if (m.holds === undefined || primary?.holds === undefined) return null
  return Math.max(0, primary.holds - m.holds)
}

/** The members a move may go to: a copy taken on another device that runs
 *  sessions. Not the primary; not an archive replica, which runs none; not a
 *  teleport's target, already being moved to. */
export function moveTargets(s: HiveSession): HiveReplicaMember[] {
  return (s.replicaset?.members ?? []).filter(
    (m) =>
      m.state === 'joined' &&
      m.role !== 'primary' &&
      m.role !== 'archive' &&
      m.role !== 'teleport' &&
      m.device_id !== s.device_id,
  )
}

/** The target to preselect: the one whose copy holds the most, since the
 *  server refuses a move to a copy that holds less than another
 *  (`not_freshest`), and counts one that reported nothing as holding none.
 *  Among equals, the first. */
export function freshestTarget(s: HiveSession): HiveReplicaMember | undefined {
  let best: HiveReplicaMember | undefined
  for (const m of moveTargets(s)) {
    if (!best || (m.holds ?? 0) > (best.holds ?? 0)) best = m
  }
  return best
}

/** A device of the organization, as the agents store lists it. */
export interface DeviceLike {
  id: string
  name: string
  display_name?: string
  presence?: 'online' | 'stale' | 'offline'
  is_online?: boolean
  capabilities?: { rpc?: string[] }
}

/** A word in what a device advertises, matched by equality: `hive` is a
 *  prefix of every hive word, and `hive-purge` of `hive-purge-all`. */
export function advertises(d: DeviceLike, word: string): boolean {
  return d.capabilities?.rpc?.includes(word) ?? false
}

function online(d: DeviceLike): boolean {
  return d.presence ? d.presence === 'online' : !!d.is_online
}

/** Whether a device holds a copy the server counts: a joined row that is not
 *  the primary's (a teleport to one is refused `already_a_member`). */
function holdsCopy(s: HiveSession, deviceId: string): boolean {
  return (s.replicaset?.members ?? []).some(
    (m) => m.device_id === deviceId && m.role !== 'primary' && m.state === 'joined',
  )
}

/** P2j-2 — the devices a teleport may go to: online, advertising
 *  `hive-teleport`, holding no copy (one that does is moved to by Move), not
 *  the device it runs on, and no archive replica, which runs no session. */
export function teleportTargets(s: HiveSession, devices: DeviceLike[]): DeviceLike[] {
  return devices.filter(
    (d) =>
      online(d) &&
      advertises(d, 'hive-teleport') &&
      !advertises(d, 'hive-archive') &&
      d.id !== s.device_id &&
      !holdsCopy(s, d.id),
  )
}

/** P2j-2 — the devices a fork may go to, each saying whether it holds a copy
 *  to fork from or takes one first (P2f-3c): online, advertising `hive-fork`,
 *  and no archive replica. The device the session runs on holds its own. */
export function forkTargets(s: HiveSession, devices: DeviceLike[]): Array<{ device: DeviceLike; holds: boolean }> {
  return devices
    .filter((d) => online(d) && advertises(d, 'hive-fork') && !advertises(d, 'hive-archive'))
    .map((d) => ({ device: d, holds: d.id === s.device_id || holdsCopy(s, d.id) }))
    .sort((a, b) => Number(b.holds) - Number(a.holds))
}

/** A copy that is no target and holds more than every target (an archive
 *  replica a laptop has not caught up with): while it does, the server
 *  refuses every move as `not_freshest`. */
export function aheadOfTargets(s: HiveSession): HiveReplicaMember | undefined {
  const targets = moveTargets(s)
  const most = Math.max(...targets.map((m) => m.holds ?? 0), 0)
  let ahead: HiveReplicaMember | undefined
  for (const m of s.replicaset?.members ?? []) {
    if (m.state !== 'joined' || m.role === 'primary' || m.device_id === s.device_id) continue
    if (targets.includes(m) || m.holds === undefined || m.holds <= most) continue
    if (!ahead || m.holds > (ahead.holds ?? 0)) ahead = m
  }
  return targets.length ? ahead : undefined
}
