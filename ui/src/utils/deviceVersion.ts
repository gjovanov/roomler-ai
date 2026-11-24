// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #3565 — a device row's version is its daemon's own. Only the daemon's
 * hello writes it, and an enrollment never does, so a row has no version
 * until its daemon first connects. The server sends `""` for such a row.
 * Every surface reads that as "not reported yet", never as a version, and
 * the "not updating" chip ignores it (`updateStall.ts`).
 */

/** What a surface shows in place of a version the daemon has not reported. */
export const VERSION_NOT_REPORTED = 'not reported yet'

/** The version the device's daemon reported, or `null` while it has not
 *  reported one (`""`, blank, or absent). */
export function reportedVersion(version: string | null | undefined): string | null {
  const v = version?.trim()
  return v ? v : null
}

/** A row's version as a label: `v0.4.197`, or `version not reported yet`. */
export function versionLabel(version: string | null | undefined): string {
  const v = reportedVersion(version)
  return v ? `v${v}` : `version ${VERSION_NOT_REPORTED}`
}
