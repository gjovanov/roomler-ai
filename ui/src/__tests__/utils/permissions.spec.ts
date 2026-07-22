import { describe, expect, it } from 'vitest'
import {
  ADMINISTRATOR,
  ALL_PERMISSIONS,
  DEFAULT_ADMIN,
  DEFAULT_MEMBER,
  PERMISSION_FLAGS,
  PERMISSION_GROUPS,
  describePermissions,
  hasPermission,
} from '@/utils/permissions'

// These tests LOCK the TS catalog to the server bitfield in
// crates/db/src/models/role.rs::permissions — if either side drifts, a
// composite value changes and this fails loudly.

describe('permission catalog', () => {
  it('defines all 27 flags with unique bits and keys', () => {
    expect(PERMISSION_FLAGS).toHaveLength(27)
    const bits = PERMISSION_FLAGS.map((f) => f.bit)
    expect(new Set(bits).size).toBe(27)
    const keys = PERMISSION_FLAGS.map((f) => f.key)
    expect(new Set(keys).size).toBe(27)
    // Every bit is a single power of two inside the defined range.
    for (const bit of bits) {
      expect(bit & (bit - 1)).toBe(0)
      expect(bit).toBeLessThanOrEqual(1 << 26)
    }
  })

  it('OR of every flag equals ALL', () => {
    const all = PERMISSION_FLAGS.reduce((m, f) => m | f.bit, 0)
    expect(all).toBe(ALL_PERMISSIONS)
    expect(ALL_PERMISSIONS).toBe((1 << 27) - 1)
  })

  it('mirrors the server shift assignments for the load-bearing flags', () => {
    const bit = (key: string) => PERMISSION_FLAGS.find((f) => f.key === key)?.bit
    expect(bit('VIEW_CHANNELS')).toBe(1 << 0)
    expect(bit('MANAGE_ROLES')).toBe(1 << 2)
    expect(ADMINISTRATOR).toBe(1 << 23)
    expect(bit('ADMINISTRATOR')).toBe(1 << 23)
    expect(bit('MANAGE_AGENTS')).toBe(1 << 24)
    expect(bit('REMOTE_CONTROL')).toBe(1 << 25)
    expect(bit('VIEW_REMOTE_AUDIT')).toBe(1 << 26)
  })

  it('DEFAULT_MEMBER matches the server composite', () => {
    const expected =
      (1 << 0) | // VIEW_CHANNELS
      (1 << 7) | // SEND_MESSAGES
      (1 << 8) | // SEND_THREADS
      (1 << 9) | // EMBED_LINKS
      (1 << 10) | // ATTACH_FILES
      (1 << 11) | // READ_HISTORY
      (1 << 14) | // ADD_REACTIONS
      (1 << 15) | // CONNECT_VOICE
      (1 << 16) | // SPEAK
      (1 << 17) // STREAM_VIDEO
    expect(DEFAULT_MEMBER).toBe(expected)
    expect(DEFAULT_MEMBER).toBe(249729)
  })

  it('DEFAULT_ADMIN matches the server composite (everything except ADMINISTRATOR)', () => {
    const expected =
      DEFAULT_MEMBER |
      (1 << 1) | // MANAGE_CHANNELS
      (1 << 2) | // MANAGE_ROLES
      (1 << 4) | // KICK_MEMBERS
      (1 << 5) | // BAN_MEMBERS
      (1 << 6) | // INVITE_MEMBERS
      (1 << 12) | // MENTION_EVERYONE
      (1 << 13) | // MANAGE_MESSAGES
      (1 << 18) | // MUTE_MEMBERS
      (1 << 19) | // DEAFEN_MEMBERS
      (1 << 20) | // MOVE_MEMBERS
      (1 << 21) | // MANAGE_MEETINGS
      (1 << 22) | // MANAGE_DOCUMENTS
      (1 << 24) | // MANAGE_AGENTS
      (1 << 25) | // REMOTE_CONTROL
      (1 << 26) // VIEW_REMOTE_AUDIT
    expect(DEFAULT_ADMIN).toBe(expected)
    expect(DEFAULT_ADMIN & ADMINISTRATOR).toBe(0)
    // Historical cross-check: the pre-remote-perms admin composite was
    // 8388599 (still live on old tenants); the current one appends the
    // three remote-control bits.
    expect(DEFAULT_ADMIN).toBe(8388599 | (1 << 24) | (1 << 25) | (1 << 26))
  })

  it('hasPermission mirrors the server ADMINISTRATOR bypass', () => {
    const remote = 1 << 25
    expect(hasPermission(ADMINISTRATOR, remote)).toBe(true)
    expect(hasPermission(remote, remote)).toBe(true)
    expect(hasPermission(remote, 1 << 2)).toBe(false)
    expect(hasPermission(0, 0)).toBe(true) // vacuous flag, same as the server
  })

  it('describePermissions lists labels in catalog order', () => {
    const labels = describePermissions((1 << 0) | (1 << 25))
    expect(labels).toEqual(['View channels', 'Remote control'])
    expect(describePermissions(0)).toEqual([])
  })

  it('groups are stable for the editor layout', () => {
    expect(PERMISSION_GROUPS).toEqual([
      'General',
      'Messaging',
      'Voice & video',
      'Moderation',
      'Remote control',
    ])
  })
})
