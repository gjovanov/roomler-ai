// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-103 — when a device's agent token needs someone's attention.
 *
 * A device renews its own token with 180 days to spare (agent 0.4.135 and
 * later), so the list stays quiet for every healthy device. A token within
 * {@link WARN_DAYS} of expiry is one that is NOT renewing: an agent too old to
 * renew, or a renewal the server refused. An expired one can still renew for
 * {@link GRACE_DAYS} days once its agent can; after that only a re-enrollment
 * brings it back.
 */
export const WARN_DAYS = 60
/** The server's renewal grace past `exp` (`RENEWAL_GRACE_SECS`). */
export const GRACE_DAYS = 90

const DAY_MS = 86_400_000

export interface CredentialChip {
  text: string
  color: 'warning' | 'error'
  tooltip: string
}

/** The chip for a token expiring at `expiresAt` (RFC 3339), or `null` when
 *  there is nothing to act on: not reported, unreadable, or comfortably far. */
export function credentialChip(
  expiresAt: string | null | undefined,
  now: number = Date.now(),
): CredentialChip | null {
  if (!expiresAt) return null
  const at = Date.parse(expiresAt)
  if (Number.isNaN(at)) return null
  const left = at - now
  const on = new Date(at).toISOString().slice(0, 10)
  if (left > WARN_DAYS * DAY_MS) return null
  if (left > 0) {
    const days = Math.floor(left / DAY_MS)
    return {
      text: days === 0 ? 'credential expires today' : `credential expires in ${days} d`,
      color: 'warning',
      tooltip:
        `Its agent token expires on ${on}. A device on agent 0.4.135 or later renews it ` +
        `itself 180 days early, so one this close is not renewing: update its agent, ` +
        `or check whether it was removed and re-enrolled elsewhere.`,
    }
  }
  const late = Math.floor(-left / DAY_MS)
  const renewable = late < GRACE_DAYS
  return {
    text: 'credential expired',
    color: 'error',
    tooltip: renewable
      ? `Its agent token expired on ${on}. The server still renews it for ` +
        `${GRACE_DAYS - late} more day(s) once the device runs agent 0.4.135 or later; ` +
        `after that, re-enroll it.`
      : `Its agent token expired on ${on}, more than ${GRACE_DAYS} days ago: ` +
        `re-enroll the device.`,
  }
}
