// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-101 P2c — what the session policy took out of this session, and why.
 *
 * The hub strips clipboard, files and audio from a grant when the org's
 * policy (P2a) or the device owner's floor (P2b) refuses them, and names each
 * in `rc:session.created.features_refused` (feature → reason). The agent
 * refuses them at the channel either way; this is so the viewer can say why
 * instead of failing when the person clicks.
 *
 * Pure functions only, so the words and the parsing are unit-tested without
 * a session.
 */

/** The three features a session policy covers, by their wire names. */
export type SessionFeature = 'clipboard' | 'files' | 'audio'

export const SESSION_FEATURES: readonly SessionFeature[] = ['clipboard', 'files', 'audio']

/** How each feature is named in a sentence (start of a sentence / inside one). */
const FEATURE_WORDS: Record<SessionFeature, { start: string; inline: string }> = {
  clipboard: { start: 'Clipboard', inline: 'the clipboard' },
  files: { start: 'File transfer', inline: 'file transfer' },
  audio: { start: 'Host audio', inline: 'host audio' },
}

/** Every reason the hub can give, for tests and for callers that branch. */
export const FEATURE_REASONS = [
  'org_policy_off',
  'device_policy_off',
  'needs_consent',
  'device_cannot_ask',
  'policy_unavailable',
] as const

/**
 * `rc:session.created.features_refused` as a plain map. Only string values
 * are kept: anything else, or no map at all (an older server), reads as
 * nothing refused. A key or reason this build does not know is kept, so a
 * newer server's refusal still disables the control.
 */
export function parseFeaturesRefused(raw: unknown): Record<string, string> {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return {}
  const out: Record<string, string> = {}
  for (const [k, v] of Object.entries(raw as Record<string, unknown>)) {
    if (typeof v === 'string' && v.length > 0) out[k] = v
  }
  return out
}

/** A reason code in words. An unknown code (a newer server) still reads as "not available". */
export function describeFeatureReason(feature: SessionFeature, code: string): string {
  const w = FEATURE_WORDS[feature]
  switch (code) {
    case 'org_policy_off':
      return `Your organization has turned off ${w.inline} for remote sessions.`
    case 'device_policy_off':
      return `This device's owner has turned off ${w.inline} for remote sessions.`
    case 'needs_consent':
      return `${w.start} needs the approval of the person at the device, and this session does not ask them.`
    case 'device_cannot_ask':
      return `${w.start} needs the person at the device to approve it, and this device's agent is too old to ask. Updating the device fixes it.`
    case 'policy_unavailable':
      return `Your organization's session policy could not be read, so ${w.inline} is off for this session. Reconnect to try again.`
    default:
      return `${w.start} is not available in this session (${code}).`
  }
}

/** Why `feature` is off in this session, in words; null when it is not. */
export function featureRefusal(
  refused: Record<string, string>,
  feature: SessionFeature,
): string | null {
  const code = refused[feature]
  return code ? describeFeatureReason(feature, code) : null
}

/**
 * One line for the session's start, naming what is off, or null when
 * nothing is. Each control carries its own full reason; this only says
 * which ones to look at.
 */
export function summarizeRefusals(refused: Record<string, string>): string | null {
  const off = SESSION_FEATURES.filter((f) => refused[f]).map((f) => FEATURE_WORDS[f].inline)
  if (off.length === 0) return null
  const list =
    off.length === 1 ? off[0] : `${off.slice(0, -1).join(', ')} and ${off[off.length - 1]}`
  return `Off for this session: ${list}. Hover a greyed control to see why.`
}
