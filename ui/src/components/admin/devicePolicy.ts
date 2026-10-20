// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * A device's access policy as the Devices page shows and edits it — the
 * choices, their help text, and the one-line summary a collapsed card or row
 * shows in place of the six controls.
 *
 * ONE definition for the desktop grid and the phone card. The two used to
 * carry their own copies of every control, and copies drift: the phone card
 * ended up showing a different name and a shorter menu than the grid.
 */
import type { AccessPolicy, ConsentMode, LockOnEnd } from '@/stores/agents'

// Per-device consent mode. Email/Push route the request to the device OWNER
// (approve-link) for unattended hosts. `prompt_then_email` runs BOTH legs in
// parallel — first answer wins — with the on-host modal bounded to the attended
// window while the emailed link keeps the full async one; a host that goes
// unanswered hands over to the owner rather than ending the session.
export const CONSENT_MODE_ITEMS: { title: string; value: ConsentMode }[] = [
  { title: 'Prompt on host (attended)', value: 'prompt' },
  { title: 'Auto-grant (unattended)', value: 'auto' },
  { title: 'Email the owner', value: 'email' },
  { title: 'Push to the owner', value: 'push' },
  { title: 'Prompt host + email owner', value: 'prompt_then_email' },
]

export function consentModeTitle(mode: ConsentMode): string {
  return CONSENT_MODE_ITEMS.find((i) => i.value === mode)?.title ?? mode
}

// P6 — per-device multi-user input arbitration mode. `free` (default) =
// every INPUT-granted viewer injects (agent-fenced); `exclusive` = one
// floor holder with request/grant. Applies to NEW sessions.
export const INPUT_MODE_ITEMS: { title: string; value: 'free' | 'exclusive' }[] = [
  { title: 'Free-for-all (fenced)', value: 'free' },
  { title: 'Exclusive (one controller)', value: 'exclusive' },
]

// FR-101 P4b-2b — this device's own idle limit, in minutes
// (`AccessPolicy.auto_terminate_idle_minutes`). `0` in the select is "none of
// its own" (stored as null), so the organization's limit alone applies.
const IDLE_MINUTE_PRESETS = [5, 15, 30, 60, 120]

/** The presets, plus a stored value that is none of them (set through the
 *  API), so the select never shows blank for a limit that is in force. */
export function idleMinuteItems(current: number | null | undefined): { title: string; value: number }[] {
  const minutes = [...IDLE_MINUTE_PRESETS]
  if (typeof current === 'number' && current > 0 && !minutes.includes(current)) {
    minutes.push(current)
    minutes.sort((a, b) => a - b)
  }
  return [
    { title: 'Idle: org limit only', value: 0 },
    ...minutes.map((m) => ({ title: `Idle: ${m} min`, value: m })),
  ]
}

export const IDLE_LIMIT_HINT =
  'End a remote-desktop session to this device after this long with no input and no ' +
  'change on its screen. The shorter of this and the organization’s limit applies, so it ' +
  'can only tighten the org’s. The device keeps it: an agent that predates it does not. ' +
  'Applies to the next session.'

// FR-99 P3b — what this device's sessions ask of it when the last one ends
// (`AccessPolicy.lock_on_end`). `never` in the select asks nothing (stored as
// null), so the device owner's own setting alone applies.
export const LOCK_ON_END_ITEMS: { title: string; value: LockOnEnd }[] = [
  { title: 'Lock: the device decides', value: 'never' },
  { title: 'Lock: if locked or idle at the start', value: 'auto' },
  { title: 'Lock: always', value: 'always' },
]

export const LOCK_ON_END_HINT =
  'Lock this device’s screen when the last remote-desktop session to it ends. ' +
  '“If locked or idle at the start” locks it again only if it was locked, or nobody was ' +
  'at it, when the session began. The device takes the stricter of this and its owner’s ' +
  'own setting, so this can only tighten it. Honoured by service installs on Windows and ' +
  'Linux, where a session can unlock the screen again; a per-user install, or an agent that ' +
  'predates it, does not take it. Applies to the next session.'

/** #1882 — what the "Remote desktop over the mesh" switch does, for its
 *  tooltip. */
export const RC_OVERLAY_HINT =
  'Off (default): remote desktop never rides the WireGuard mesh; it connects ' +
  'over the LAN, a direct path or a TURN relay. On: it may take the mesh when ' +
  'that is the best path, and inherits the mesh’s outages. Applies to the ' +
  'next session.'

/** The label says what is TRUE right now, not what the checkbox is called —
 *  "you own this device, so consent is currently skipped" is the fact an
 *  operator needs; "Ask me too" alone would leave them guessing what the
 *  unchecked state means. */
export function ownerPromptLabel(policy: AccessPolicy): string {
  const mode = policy.consent_mode ?? 'prompt'
  return policy.prompt_owner
    ? `Ask me too (you own this device; "${consentModeTitle(mode)}" applies to you)`
    : 'Ask me too (you own this device, so consent is skipped for you)'
}

/** Short words for the consent mode, for a summary line where the select's
 *  full title would not fit. */
const CONSENT_SHORT: Record<ConsentMode, string> = {
  prompt: 'Prompt on host',
  auto: 'Auto-grant',
  email: 'Email owner',
  push: 'Push owner',
  prompt_then_email: 'Prompt + email',
}

/**
 * What a collapsed policy says, in a few words: the consent mode always (it is
 * the one setting every device has an answer for), then only the settings
 * that DIFFER from their defaults — a summary that repeats every default on
 * every row is noise, and the point is to show what someone changed.
 *
 * `ownedByMe` adds the owner's own exemption, because for the owner it is the
 * fact that decides whether the consent mode applies at all (FR-27) — except
 * under auto-grant, where nobody is asked anyway and saying so is noise.
 */
export function policySummary(policy: AccessPolicy | null | undefined, ownedByMe: boolean): string[] {
  if (!policy) return []
  const mode = policy.consent_mode ?? 'prompt'
  const out = [CONSENT_SHORT[mode] ?? mode]
  if (ownedByMe && !policy.prompt_owner && mode !== 'auto') out.push('skipped for you')
  if ((policy.input_mode ?? 'free') === 'exclusive') out.push('one controller')
  const idle = policy.auto_terminate_idle_minutes
  if (typeof idle === 'number' && idle > 0) out.push(`idle ${idle} min`)
  if (policy.lock_on_end === 'auto') out.push('lock if locked at start')
  if (policy.lock_on_end === 'always') out.push('lock at end')
  if (policy.rc_overlay) out.push('over the mesh')
  return out
}
