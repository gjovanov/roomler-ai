/**
 * The tenant permission catalog — a TS mirror of the server's bitfield in
 * `crates/db/src/models/role.rs::permissions`. Bits are ≤ 26, so plain JS
 * 32-bit bitwise ops are safe (the wire carries the mask as a number).
 *
 * Keep this file in lockstep with the Rust module: the unit test locks the
 * composite values so drift is caught at `bun run test:unit`, not in prod.
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

/** All 27 defined flags, grouped for the role-editor UI. */
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

/** Server `DEFAULT_ADMIN` composite — everything except ADMINISTRATOR. */
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
  ])

/** Every defined bit — server `ALL = (1 << 27) - 1`. */
export const ALL_PERMISSIONS = (1 << 27) - 1

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
