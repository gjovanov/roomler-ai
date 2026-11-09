// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD

/**
 * #3327 — whether a refused delete means the thing is already gone.
 *
 * The server resolves a message or a file by id as a LIVE row only, so a
 * delete of one already deleted answers 404, as does one in a room this
 * account can no longer read. Either way what the person asked for has
 * happened: the row leaves the list and no error is shown. Any other refusal
 * (a 403 for someone else's message, a 5xx) is still an error.
 */
export function alreadyDeleted(e: unknown): boolean {
  return (e as { status?: number } | null)?.status === 404
}
