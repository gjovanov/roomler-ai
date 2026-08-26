/**
 * The tenant permission catalog — a TS mirror of the server's bitfield in
 * `crates/db/src/models/role.rs::permissions`.
 *
 * Keep this file in lockstep with the Rust module: the unit test locks the
 * composite values so drift is caught at `bun run test:unit`, not in prod.
 * Drift is not cosmetic — the role editor writes back the mask it built from
 * THIS catalog, so a bit missing here is a bit silently stripped from any
 * role that gets saved.
 *
 * ⚠️ JS bitwise operands are coerced to **signed int32**. Bits 0–30 are safe
 * (`1 << 30` is positive), but composites must not be built with a shift:
 * `(1 << 31) - 1` is −2147483649 in JS, not 2147483647. Hence `2 ** 31 - 1`
 * below. A future bit 31 would make `1 << 31` negative and break every mask
 * operation in this file — it needs BigInt or string masks, not a new entry.
 */

export interface PermissionFlag {
  /** SCREAMING_SNAKE key, identical to the Rust constant name. */
  key: string
  /** The bit VALUE (1 << n), not the shift. */
  bit: number
  label: string
  group: string
  description?: string
}

export const ADMINISTRATOR = 1 << 23

/** All 31 defined flags, grouped for the role-editor UI. */
export const PERMISSION_FLAGS: PermissionFlag[] = [
  // ── General ──────────────────────────────────────────────────────
  { key: 'VIEW_CHANNELS', bit: 1 << 0, label: 'View channels', group: 'General' },
  { key: 'MANAGE_CHANNELS', bit: 1 << 1, label: 'Manage channels', group: 'General' },
  { key: 'MANAGE_ROLES', bit: 1 << 2, label: 'Manage roles', group: 'General' },
  { key: 'MANAGE_TENANT', bit: 1 << 3, label: 'Manage tenant', group: 'General' },
  { key: 'INVITE_MEMBERS', bit: 1 << 6, label: 'Invite members', group: 'General' },
  {
    key: 'ADMINISTRATOR',
    bit: ADMINISTRATOR,
    label: 'Administrator',
    group: 'General',
    description: 'Bypasses ALL permission checks — grants everything below implicitly.',
  },
  // ── Messaging ────────────────────────────────────────────────────
  { key: 'SEND_MESSAGES', bit: 1 << 7, label: 'Send messages', group: 'Messaging' },
  { key: 'SEND_THREADS', bit: 1 << 8, label: 'Create threads', group: 'Messaging' },
  { key: 'EMBED_LINKS', bit: 1 << 9, label: 'Embed links', group: 'Messaging' },
  { key: 'ATTACH_FILES', bit: 1 << 10, label: 'Attach files', group: 'Messaging' },
  { key: 'READ_HISTORY', bit: 1 << 11, label: 'Read history', group: 'Messaging' },
  { key: 'MENTION_EVERYONE', bit: 1 << 12, label: 'Mention @everyone', group: 'Messaging' },
  { key: 'MANAGE_MESSAGES', bit: 1 << 13, label: 'Manage messages', group: 'Messaging' },
  { key: 'ADD_REACTIONS', bit: 1 << 14, label: 'Add reactions', group: 'Messaging' },
  // ── Voice & video ────────────────────────────────────────────────
  { key: 'CONNECT_VOICE', bit: 1 << 15, label: 'Connect to voice', group: 'Voice & video' },
  { key: 'SPEAK', bit: 1 << 16, label: 'Speak', group: 'Voice & video' },
  { key: 'STREAM_VIDEO', bit: 1 << 17, label: 'Stream video', group: 'Voice & video' },
  { key: 'MANAGE_MEETINGS', bit: 1 << 21, label: 'Manage meetings', group: 'Voice & video' },
  // ── Moderation ───────────────────────────────────────────────────
  { key: 'KICK_MEMBERS', bit: 1 << 4, label: 'Kick members', group: 'Moderation' },
  { key: 'BAN_MEMBERS', bit: 1 << 5, label: 'Ban members', group: 'Moderation' },
  { key: 'MUTE_MEMBERS', bit: 1 << 18, label: 'Mute members', group: 'Moderation' },
  { key: 'DEAFEN_MEMBERS', bit: 1 << 19, label: 'Deafen members', group: 'Moderation' },
  { key: 'MOVE_MEMBERS', bit: 1 << 20, label: 'Move members', group: 'Moderation' },
  { key: 'MANAGE_DOCUMENTS', bit: 1 << 22, label: 'Manage documents', group: 'Moderation' },
  // ── Remote control ───────────────────────────────────────────────
  {
    key: 'MANAGE_AGENTS',
    bit: 1 << 24,
    label: 'Manage devices',
    group: 'Remote control',
    description: 'Enroll, rename, delete, reassign and set policy for remote-control devices.',
  },
  {
    key: 'REMOTE_CONTROL',
    bit: 1 << 25,
    label: 'Remote control',
    group: 'Remote control',
    description: "Start remote-control sessions against devices the user does NOT own (controlling one's own device never needs this).",
  },
  {
    key: 'VIEW_REMOTE_AUDIT',
    bit: 1 << 26,
    label: 'View remote audit log',
    group: 'Remote control',
    description: 'Read the remote-control session audit trail.',
  },
  // ── Fleet access ─────────────────────────────────────────────────
  // Deliberately NOT implied by "Manage devices", and deliberately not part
  // of the admin preset: managing a device's metadata and holding a root
  // shell on it are different powers. Both run as SYSTEM/root with nobody
  // at the machine watching, which is why the audit views are separate bits.
  {
    key: 'EXEC_DEVICE',
    bit: 1 << 27,
    label: 'Run commands on devices',
    group: 'Fleet access',
    description:
      'Run a command on a device via Fleet RPC. Commands execute as SYSTEM (Windows) or root (Linux). Also requires the org switch, the device policy, and the device-local key.',
  },
  {
    key: 'VIEW_EXEC_AUDIT',
    bit: 1 << 28,
    label: 'View command audit log',
    group: 'Fleet access',
    description: 'Read the Fleet-RPC audit trail — who ran what, where.',
  },
  {
    key: 'SSH_DEVICE',
    bit: 1 << 29,
    label: 'SSH to devices',
    group: 'Fleet access',
    description:
      'Open an interactive roomler SSH session to a device. Strictly more than a single command: it lasts, and it carries file transfer.',
  },
  {
    key: 'VIEW_SSH_AUDIT',
    bit: 1 << 30,
    label: 'View SSH audit log',
    group: 'Fleet access',
    description: 'Read the roomler-SSH audit trail — who was granted, or refused, a session.',
  },
]

/** Group names in display order (insertion order of the flags above). */
export const PERMISSION_GROUPS: string[] = [...new Set(PERMISSION_FLAGS.map((f) => f.group))]

function byKeys(keys: string[]): number {
  return keys.reduce((mask, key) => {
    const flag = PERMISSION_FLAGS.find((f) => f.key === key)
    if (!flag) throw new Error(`unknown permission key: ${key}`)
    return mask | flag.bit
  }, 0)
}

/** Server `DEFAULT_MEMBER` composite — the sensible baseline role. */
export const DEFAULT_MEMBER = byKeys([
  'VIEW_CHANNELS',
  'SEND_MESSAGES',
  'SEND_THREADS',
  'EMBED_LINKS',
  'ATTACH_FILES',
  'READ_HISTORY',
  'ADD_REACTIONS',
  'CONNECT_VOICE',
  'SPEAK',
  'STREAM_VIDEO',
])

/**
 * Server `DEFAULT_ADMIN` composite. NOT "everything except ADMINISTRATOR":
 * `EXEC_DEVICE` and `SSH_DEVICE` are withheld on purpose so that opening a
 * root shell on a fleet device stays an explicit grant. The matching audit
 * views ARE included — seeing what the fleet served is a different job from
 * being able to serve it.
 */
export const DEFAULT_ADMIN =
  DEFAULT_MEMBER |
  byKeys([
    'MANAGE_CHANNELS',
    'MANAGE_ROLES',
    'KICK_MEMBERS',
    'BAN_MEMBERS',
    'INVITE_MEMBERS',
    'MENTION_EVERYONE',
    'MANAGE_MESSAGES',
    'MUTE_MEMBERS',
    'DEAFEN_MEMBERS',
    'MOVE_MEMBERS',
    'MANAGE_MEETINGS',
    'MANAGE_DOCUMENTS',
    'MANAGE_AGENTS',
    'REMOTE_CONTROL',
    'VIEW_REMOTE_AUDIT',
    'VIEW_EXEC_AUDIT',
    'VIEW_SSH_AUDIT',
  ])

/**
 * Every defined bit — server `ALL = (1 << 31) - 1` = bits 0–30.
 * Written as `2 ** 31 - 1` because the Rust spelling evaluates to a NEGATIVE
 * number in JS (see the int32 note at the top of this file).
 */
export const ALL_PERMISSIONS = 2 ** 31 - 1

/**
 * Mirror of the server check: ADMINISTRATOR bypasses everything, else the
 * flag's bits must all be present.
 */
export function hasPermission(mask: number, flag: number): boolean {
  return (mask & ADMINISTRATOR) !== 0 || (mask & flag) === flag
}

/** Labels of the flags present in `mask`, in catalog order. */
export function describePermissions(mask: number): string[] {
  return PERMISSION_FLAGS.filter((f) => (mask & f.bit) !== 0).map((f) => f.label)
}

/**
 * Nav-gating predicate for the fleet surfaces (Devices page + Network
 * group). Deliberately FAIL-OPEN: while the membership hasn't loaded yet
 * (`mask === null` — first paint, endpoint hiccup, older server without
 * /member/me) the nav stays visible; the server still enforces every
 * action, so the worst case of failing open is a visible link to a page
 * whose actions 403. Failing closed would hide navigation behind a
 * network round-trip on every load.
 */
export function canSeeFleetNav(mask: number | null, isOwner: boolean): boolean {
  if (mask === null) return true
  if (isOwner) return true
  return (
    hasPermission(mask, byKeys(['MANAGE_AGENTS'])) ||
    hasPermission(mask, byKeys(['REMOTE_CONTROL']))
  )
}

/**
 * Gate for the org Analytics queries (stats PR-4). Unlike
 * `canSeeFleetNav` this is deliberately FAIL-CLOSED on `mask === null`:
 * the stats endpoints answer 404 to a caller without MANAGE_AGENTS, and
 * more importantly the api client force-logs-out on any 403 — so the UI
 * must never fire an analytics request it isn't sure is authorized.
 * Server gate: `MANAGE_AGENTS` (owner/ADMINISTRATOR bypass included).
 */
export function canQueryAnalytics(mask: number | null, isOwner: boolean): boolean {
  if (isOwner) return true
  if (mask === null) return false
  return hasPermission(mask, byKeys(['MANAGE_AGENTS']))
}

/**
 * Nav gate for the Invites surface (sidebar item + dashboard card).
 * Deliberately FAIL-CLOSED on `mask === null`, like {@link canQueryAnalytics}
 * and unlike {@link canSeeFleetNav}: `list_invites` requires INVITE_MEMBERS
 * and the api client force-logs-out on any GET 403 — so an ungated entry
 * turned a plain member's click into a logout, which users read as "the
 * invite page disappeared". Server gate: `INVITE_MEMBERS`
 * (owner/ADMINISTRATOR bypass included).
 */
export function canManageInvites(mask: number | null, isOwner: boolean): boolean {
  if (isOwner) return true
  if (mask === null) return false
  return hasPermission(mask, byKeys(['INVITE_MEMBERS']))
}

/**
 * May this caller ask a device to turn exec / SSH ON?
 *
 * `MANAGE_AGENTS` alone is NOT enough, and that is the whole point. Enabling
 * exec on a device is *granting a power*, and #600/#605 established the rule:
 * **you cannot grant a permission you do not hold.** An admin without
 * `EXEC_DEVICE` must not be able to open exec on a fleet device for somebody
 * else — that is the same escalation those PRs closed, one indirection away.
 * `EXEC_DEVICE` and `SSH_DEVICE` are deliberately absent from `DEFAULT_ADMIN`,
 * which is what makes this a real constraint rather than a formality.
 *
 * Mirrors `remote_config::decide` server-side. This only avoids offering a
 * control whose save would 403 — the server is where it is enforced.
 *
 * Deliberately FAIL-CLOSED on `mask === null`, like {@link canQueryAnalytics}
 * and unlike {@link canSeeFleetNav}: the failure mode of guessing wrong here
 * is an operator who flips a switch, gets a 403, and cannot tell whether the
 * device refused or they did.
 */
export function canGrantDeviceExec(mask: number | null, isOwner: boolean): boolean {
  if (isOwner) return true
  if (mask === null) return false
  return hasPermission(mask, byKeys(['MANAGE_AGENTS', 'EXEC_DEVICE']))
}

/** {@link canGrantDeviceExec}'s twin. A SEPARATE bit on purpose: an SSH
 *  session is strictly more than a bounded command, so holding one must never
 *  imply the other. */
export function canGrantDeviceSsh(mask: number | null, isOwner: boolean): boolean {
  if (isOwner) return true
  if (mask === null) return false
  return hasPermission(mask, byKeys(['MANAGE_AGENTS', 'SSH_DEVICE']))
}
