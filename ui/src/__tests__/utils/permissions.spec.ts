// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import {
  ADMINISTRATOR,
  ALL_PERMISSIONS,
  maskClear,
  maskHas,
  maskSet,
  maskUnion,
  DEFAULT_ADMIN,
  DEFAULT_MEMBER,
  PERMISSION_FLAGS,
  PERMISSION_GROUPS,
  canGrantDeviceExec,
  canGrantDeviceSsh,
  canManageInvites,
  canViewExecAudit,
  canViewSshAudit,
  canQueryAnalytics,
  canSeeFleetNav,
  describePermissions,
  hasPermission,
} from '@/utils/permissions'

// These tests LOCK the TS catalog to the server bitfield in
// crates/db/src/models/role.rs::permissions — if either side drifts, a
// composite value changes and this fails loudly.

describe('permission catalog', () => {
  it('defines all 31 flags with unique bits and keys', () => {
    expect(PERMISSION_FLAGS).toHaveLength(31)
    const bits = PERMISSION_FLAGS.map((f) => f.bit)
    expect(new Set(bits).size).toBe(31)
    const keys = PERMISSION_FLAGS.map((f) => f.key)
    expect(new Set(keys).size).toBe(31)
    // Every bit is a single power of two. The ceiling is now bit 52, not 30
    // (#888): the file uses arithmetic rather than bitwise operators, so the
    // limit is the JSON number's exact-integer range, not int32 coercion.
    // Still capped here so an entry above 2^52 fails loudly rather than
    // silently losing precision.
    for (const bit of bits) {
      expect(Number.isInteger(Math.log2(bit))).toBe(true)
      expect(bit).toBeGreaterThan(0)
      expect(bit).toBeLessThanOrEqual(2 ** 52)
    }
  })

  it('OR of every flag equals ALL', () => {
    const all = PERMISSION_FLAGS.reduce((m, f) => m | f.bit, 0)
    expect(all).toBe(ALL_PERMISSIONS)
    // `(1 << 31) - 1` — the Rust spelling — is −2147483649 in JS. Assert the
    // POSITIVE value so a copy-paste of the Rust expression fails here.
    expect(ALL_PERMISSIONS).toBe(2147483647)
    expect(ALL_PERMISSIONS).toBe(2 ** 31 - 1)
    expect(ALL_PERMISSIONS).toBeGreaterThan(0)
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
    expect(bit('EXEC_DEVICE')).toBe(1 << 27)
    expect(bit('VIEW_EXEC_AUDIT')).toBe(1 << 28)
    expect(bit('SSH_DEVICE')).toBe(1 << 29)
    expect(bit('VIEW_SSH_AUDIT')).toBe(1 << 30)
  })

  it('withholds the fleet-access grants from the admin preset', () => {
    // Mirrors `ssh_and_exec_are_independently_grantable` +
    // `admins_can_read_the_audits_without_gaining_the_powers` on the server.
    // The preset buttons in RolesSection OVERWRITE the mask, so if these ever
    // crept into DEFAULT_ADMIN, one click would hand every admin a root shell
    // on every device in the fleet.
    expect(DEFAULT_ADMIN & (1 << 27)).toBe(0) // EXEC_DEVICE
    expect(DEFAULT_ADMIN & (1 << 29)).toBe(0) // SSH_DEVICE
    // ...while the audit views ARE granted: reviewing what the fleet served
    // is a different job from being able to serve it.
    expect(DEFAULT_ADMIN & (1 << 28)).not.toBe(0) // VIEW_EXEC_AUDIT
    expect(DEFAULT_ADMIN & (1 << 30)).not.toBe(0) // VIEW_SSH_AUDIT
  })

  it('a preset round-trip preserves every bit the preset claims to set', () => {
    // The regression this file failed to catch: the catalog lagged the server
    // by four bits, so `applyPreset` — which assigns the composite wholesale —
    // silently dropped them. Any bit in a preset must survive being described
    // and re-derived from the catalog.
    for (const preset of [DEFAULT_MEMBER, DEFAULT_ADMIN, ALL_PERMISSIONS]) {
      const covered = PERMISSION_FLAGS.reduce(
        (m, f) => ((preset & f.bit) !== 0 ? m | f.bit : m),
        0,
      )
      expect(covered).toBe(preset)
    }
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

  it('DEFAULT_ADMIN matches the server composite', () => {
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
      (1 << 26) | // VIEW_REMOTE_AUDIT
      (1 << 28) | // VIEW_EXEC_AUDIT   (the grant, EXEC_DEVICE, is withheld)
      (1 << 30) // VIEW_SSH_AUDIT    (the grant, SSH_DEVICE, is withheld)
    expect(DEFAULT_ADMIN).toBe(expected)
    expect(DEFAULT_ADMIN & ADMINISTRATOR).toBe(0)
    // Historical cross-check: the pre-remote-perms admin composite was
    // 8388599 (still live on old tenants); the current one appends the
    // three remote-control bits plus the two fleet AUDIT views.
    expect(DEFAULT_ADMIN).toBe(
      8388599 | (1 << 24) | (1 << 25) | (1 << 26) | (1 << 28) | (1 << 30),
    )
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
      'Fleet access',
    ])
  })
})

describe('canSeeFleetNav (Devices + Network nav gating)', () => {
  const MANAGE_AGENTS = 1 << 24
  const REMOTE_CONTROL = 1 << 25

  it('fails OPEN while the membership has not loaded (null mask)', () => {
    expect(canSeeFleetNav(null, false)).toBe(true)
  })

  it('shows for either fleet permission alone', () => {
    expect(canSeeFleetNav(MANAGE_AGENTS, false)).toBe(true)
    expect(canSeeFleetNav(REMOTE_CONTROL, false)).toBe(true)
  })

  it('shows for ADMINISTRATOR and for the tenant owner regardless of mask', () => {
    expect(canSeeFleetNav(ADMINISTRATOR, false)).toBe(true)
    expect(canSeeFleetNav(0, true)).toBe(true)
  })

  it('hides for a plain member (default member mask has no fleet bits)', () => {
    expect(canSeeFleetNav(DEFAULT_MEMBER, false)).toBe(false)
    expect(canSeeFleetNav(0, false)).toBe(false)
  })

  it('DEFAULT_ADMIN sees fleet nav (sanity vs the role preset)', () => {
    expect(canSeeFleetNav(DEFAULT_ADMIN, false)).toBe(true)
  })
})

describe('canQueryAnalytics (org analytics gating — stats PR-4)', () => {
  const MANAGE_AGENTS = 1 << 24

  it('fails CLOSED while the membership has not loaded (null mask)', () => {
    // Opposite convention to canSeeFleetNav — the analytics endpoints
    // 404 without MANAGE_AGENTS and the api client logs out on 403, so
    // the UI must never optimistically fire a query.
    expect(canQueryAnalytics(null, false)).toBe(false)
  })

  it('owner always allowed, even before the mask loads', () => {
    expect(canQueryAnalytics(null, true)).toBe(true)
    expect(canQueryAnalytics(0, true)).toBe(true)
  })

  it('MANAGE_AGENTS or ADMINISTRATOR allowed', () => {
    expect(canQueryAnalytics(MANAGE_AGENTS, false)).toBe(true)
    expect(canQueryAnalytics(ADMINISTRATOR, false)).toBe(true)
    expect(canQueryAnalytics(DEFAULT_ADMIN, false)).toBe(true)
  })

  it('plain member denied', () => {
    expect(canQueryAnalytics(DEFAULT_MEMBER, false)).toBe(false)
    expect(canQueryAnalytics(0, false)).toBe(false)
  })
})

describe('canViewExecAudit / canViewSshAudit (Audit nav gating)', () => {
  const VIEW_EXEC_AUDIT = 1 << 28
  const VIEW_SSH_AUDIT = 1 << 30

  it('fail CLOSED on a null mask (403 ⇒ forced logout)', () => {
    expect(canViewExecAudit(null, false)).toBe(false)
    expect(canViewSshAudit(null, false)).toBe(false)
    expect(canViewExecAudit(null, true)).toBe(true)
    expect(canViewSshAudit(null, true)).toBe(true)
  })

  it('the two bits are independent — one never implies the other', () => {
    expect(canViewExecAudit(VIEW_EXEC_AUDIT, false)).toBe(true)
    expect(canViewExecAudit(VIEW_SSH_AUDIT, false)).toBe(false)
    expect(canViewSshAudit(VIEW_SSH_AUDIT, false)).toBe(true)
    expect(canViewSshAudit(VIEW_EXEC_AUDIT, false)).toBe(false)
  })

  it('ADMINISTRATOR and DEFAULT_ADMIN pass both; plain member neither', () => {
    expect(canViewExecAudit(ADMINISTRATOR, false)).toBe(true)
    expect(canViewSshAudit(ADMINISTRATOR, false)).toBe(true)
    expect(canViewExecAudit(DEFAULT_ADMIN, false)).toBe(true)
    expect(canViewSshAudit(DEFAULT_ADMIN, false)).toBe(true)
    expect(canViewExecAudit(DEFAULT_MEMBER, false)).toBe(false)
    expect(canViewSshAudit(DEFAULT_MEMBER, false)).toBe(false)
  })
})

describe('canManageInvites (invites nav gating)', () => {
  const INVITE_MEMBERS = 1 << 6

  it('fails CLOSED while the membership has not loaded (null mask)', () => {
    // Same convention as canQueryAnalytics: list_invites needs
    // INVITE_MEMBERS and the api client logs out on GET 403 — an
    // optimistic nav entry turns a member's click into a logout.
    expect(canManageInvites(null, false)).toBe(false)
  })

  it('owner always allowed, even before the mask loads', () => {
    expect(canManageInvites(null, true)).toBe(true)
    expect(canManageInvites(0, true)).toBe(true)
  })

  it('INVITE_MEMBERS or ADMINISTRATOR allowed — DEFAULT_ADMIN carries it', () => {
    expect(canManageInvites(INVITE_MEMBERS, false)).toBe(true)
    expect(canManageInvites(ADMINISTRATOR, false)).toBe(true)
    expect(canManageInvites(DEFAULT_ADMIN, false)).toBe(true)
  })

  it('plain member denied — INVITE_MEMBERS is deliberately not in DEFAULT_MEMBER', () => {
    expect(canManageInvites(DEFAULT_MEMBER, false)).toBe(false)
    expect(canManageInvites(0, false)).toBe(false)
  })
})

describe('canGrantDeviceExec / canGrantDeviceSsh', () => {
  const MANAGE_AGENTS = 1 << 24
  const EXEC_DEVICE = 1 << 27
  const SSH_DEVICE = 1 << 29

  /// The rule #600/#605 established: you cannot grant a permission you do not
  /// hold. `DEFAULT_ADMIN` carries MANAGE_AGENTS and NEITHER grant bit, which
  /// is exactly what makes this a real constraint — if the bits were in the
  /// default admin mask, this whole check would be a formality.
  it('MANAGE_AGENTS alone cannot open exec or SSH on a device', () => {
    expect(hasPermission(DEFAULT_ADMIN, MANAGE_AGENTS)).toBe(true)
    expect(canGrantDeviceExec(DEFAULT_ADMIN, false)).toBe(false)
    expect(canGrantDeviceSsh(DEFAULT_ADMIN, false)).toBe(false)
  })

  it('the matching grant bit, on top of MANAGE_AGENTS, is what it takes', () => {
    expect(canGrantDeviceExec(DEFAULT_ADMIN | EXEC_DEVICE, false)).toBe(true)
    expect(canGrantDeviceSsh(DEFAULT_ADMIN | SSH_DEVICE, false)).toBe(true)
    // …and the grant bit alone is not enough: managing a device at all is a
    // MANAGE_AGENTS act.
    expect(canGrantDeviceExec(EXEC_DEVICE, false)).toBe(false)
    expect(canGrantDeviceSsh(SSH_DEVICE, false)).toBe(false)
  })

  it('the two grants never substitute for each other', () => {
    // A separate bit precisely because an SSH session is strictly more than a
    // bounded command.
    expect(canGrantDeviceSsh(DEFAULT_ADMIN | EXEC_DEVICE, false)).toBe(false)
    expect(canGrantDeviceExec(DEFAULT_ADMIN | SSH_DEVICE, false)).toBe(false)
  })

  it('ADMINISTRATOR and owner bypass, as everywhere else', () => {
    expect(canGrantDeviceExec(ADMINISTRATOR, false)).toBe(true)
    expect(canGrantDeviceSsh(ADMINISTRATOR, false)).toBe(true)
    expect(canGrantDeviceExec(null, true)).toBe(true)
    expect(canGrantDeviceSsh(0, true)).toBe(true)
  })

  it('fails CLOSED before the mask loads', () => {
    // Guessing "allowed" here shows a switch whose save 403s, and the operator
    // cannot tell whether the DEVICE refused or they did.
    expect(canGrantDeviceExec(null, false)).toBe(false)
    expect(canGrantDeviceSsh(null, false)).toBe(false)
  })
})

describe('#888 — the ceiling is bit 52, not bit 30', () => {
  // The bug was never the wire and never the server (u64): it was that every
  // mask operation used a JS bitwise operator, and those coerce to signed
  // int32. These pin the arithmetic helpers across the range that unlocks.
  const HIGH = [2 ** 31, 2 ** 40, 2 ** 52]

  it('the OLD bitwise approach really does break there — the reason this exists', () => {
    for (const bit of HIGH) {
      // `mask & bit` is how every check in this file used to be written.
      expect((bit & bit) === bit).toBe(false)
    }
    // ...and bit 30 is exactly where it still worked, which is why the ceiling
    // sat there rather than anywhere principled.
    expect(((1 << 30) & (1 << 30)) === 1 << 30).toBe(true)
  })

  it('maskHas reads high bits the bitwise version could not', () => {
    for (const bit of HIGH) {
      expect(maskHas(bit, bit)).toBe(true)
      expect(maskHas(0, bit)).toBe(false)
      // and does not confuse a neighbouring bit for it
      expect(maskHas(bit * 2, bit)).toBe(false)
    }
  })

  it('maskSet / maskClear round-trip a high bit without disturbing the rest', () => {
    for (const bit of HIGH) {
      const base = DEFAULT_ADMIN
      const set = maskSet(base, bit)
      expect(maskHas(set, bit)).toBe(true)
      // every previously-held permission survives — the failure mode here is a
      // saved role coming back with permissions silently stripped
      expect(describePermissions(set)).toEqual(describePermissions(base))
      expect(maskClear(set, bit)).toBe(base)
      // idempotent in both directions
      expect(maskSet(set, bit)).toBe(set)
      expect(maskClear(base, bit)).toBe(base)
    }
  })

  it('hasPermission honours a high-bit flag, and ADMINISTRATOR still bypasses', () => {
    for (const bit of HIGH) {
      expect(hasPermission(bit, bit)).toBe(true)
      expect(hasPermission(0, bit)).toBe(false)
      expect(hasPermission(ADMINISTRATOR, bit)).toBe(true)
    }
  })

  it('a high-bit mask survives the wire — JSON carries it exactly', () => {
    for (const bit of HIGH) {
      const mask = maskSet(DEFAULT_ADMIN, bit)
      const round = JSON.parse(JSON.stringify({ permissions: mask })).permissions
      expect(round).toBe(mask)
      expect(Number.isSafeInteger(round)).toBe(true)
    }
  })

  it('maskUnion composes disjoint and overlapping masks like `|` used to', () => {
    expect(maskUnion(0b0101, 0b0011)).toBe(0b0111)
    expect(maskUnion(DEFAULT_MEMBER, DEFAULT_MEMBER)).toBe(DEFAULT_MEMBER)
    expect(maskHas(maskUnion(DEFAULT_MEMBER, 2 ** 45), 2 ** 45)).toBe(true)
  })
})
