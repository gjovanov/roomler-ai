// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-90 P2j-1 — reading a session's copies by the server's own count
// (`holds`): how far behind each copy is, which members a move may go to,
// which one it should, and a copy ahead of every target.
import { describe, expect, it } from 'vitest'
import type { HiveReplicaMember, HiveSession } from '@/stores/hive'
import { aheadOfTargets, behind, freshestTarget, moveTargets, primaryOf } from '@/utils/hiveCopies'

/** A copy; its tip is where it ends, which past a fence's end holds less
 *  than its `seq` says (`at` overrides it). */
const member = (
  device_id: string,
  role: string,
  state: string,
  holds?: number,
  at?: { fence: number; seq: number },
): HiveReplicaMember => ({
  device_id,
  role,
  state,
  ...(holds === undefined ? {} : { holds, tip: { fence: at?.fence ?? 2, seq: at?.seq ?? holds, hash: 'ab', at: '' } }),
})

function session(members: HiveReplicaMember[]): HiveSession {
  return {
    id: 's1',
    owner_id: 'u1',
    title: 't',
    harness: 'claude-code',
    harness_session: 'h',
    device_id: 'p',
    device_name: 'laptop',
    folder: '/w',
    status: 'running',
    fence: 2,
    created_at: '',
    updated_at: '',
    replicaset: { policy_revision: 1, min: 2, max: 4, restricted_tags: [], members, placed_at: '' },
  }
}

describe('hiveCopies (FR-90 P2j-1)', () => {
  it('counts how far behind the primary a copy is, from what each holds', () => {
    const s = session([
      member('p', 'primary', 'joined', 40),
      member('a', 'owner', 'joined', 31),
      member('b', 'owner', 'pending'),
      // A tail past fence 1's end counts for nothing: it holds 20, whatever its seq says.
      member('y', 'owner', 'joined', 20, { fence: 1, seq: 60 }),
    ])
    const p = primaryOf(s)
    expect(p?.device_id).toBe('p')
    expect(behind(s.replicaset!.members[1], p)).toBe(9)
    expect(behind(s.replicaset!.members[3], p), "what it holds, never its tip's seq").toBe(20)
    expect(behind(s.replicaset!.members[0], p)).toBe(0)
    expect(behind(s.replicaset!.members[2], p), 'no tip, no count').toBeNull()
    expect(behind(member('x', 'owner', 'joined', 50), p), 'never below zero').toBe(0)
    expect(behind(s.replicaset!.members[1], undefined)).toBeNull()
  })

  it('moves only to a copy taken on another device that runs sessions', () => {
    const s = session([
      member('p', 'primary', 'joined', 40),
      member('a', 'owner', 'joined', 31),
      member('arch', 'archive', 'joined', 40),
      member('t', 'teleport', 'joined', 12),
      member('b', 'owner', 'pending'),
      member('c', 'owner', 'refused'),
      member('d', 'owner', 'leaving', 39),
    ])
    expect(moveTargets(s).map((m) => m.device_id), 'not the primary, an archive, a teleport, nor one not joined').toEqual(['a'])
  })

  it('preselects the copy that holds the most, the first among equals, and counts none reported as none', () => {
    const s = session([
      member('p', 'primary', 'joined', 40),
      member('a', 'owner', 'joined', 31),
      member('b', 'owner', 'joined', 38),
      member('d', 'owner', 'joined'),
      // Its seq is the highest, past where fence 1 ended: it holds less.
      member('x', 'owner', 'joined', 30, { fence: 1, seq: 45 }),
    ])
    expect(freshestTarget(s)?.device_id, 'by what it holds, never by its seq').toBe('b')
    const ties = session([
      member('p', 'primary', 'joined', 40),
      member('b', 'owner', 'joined', 38),
      member('c', 'owner', 'joined', 38),
    ])
    expect(freshestTarget(ties)?.device_id, 'the first of two that hold as much').toBe('b')
    const none = session([member('p', 'primary', 'joined', 40), member('d', 'owner', 'joined'), member('e', 'owner', 'joined')])
    expect(freshestTarget(none)?.device_id, 'none said: the first').toBe('d')
    expect(freshestTarget(session([member('p', 'primary', 'joined', 40)]))).toBeUndefined()
  })

  it('names a copy that holds more than every target, which the server would refuse a move for', () => {
    const s = session([
      member('p', 'primary', 'joined', 40),
      member('a', 'owner', 'joined', 31),
      member('arch', 'archive', 'joined', 40),
    ])
    expect(aheadOfTargets(s)?.device_id).toBe('arch')
    const even = session([member('p', 'primary', 'joined', 40), member('a', 'owner', 'joined', 40), member('arch', 'archive', 'joined', 40)])
    expect(aheadOfTargets(even), 'as much is not more').toBeUndefined()
    const noTarget = session([member('p', 'primary', 'joined', 40), member('arch', 'archive', 'joined', 40)])
    expect(aheadOfTargets(noTarget), 'no target, nothing to refuse').toBeUndefined()
  })
})
