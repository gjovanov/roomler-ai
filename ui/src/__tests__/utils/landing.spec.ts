// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import { DOWNLOAD, downloadLead } from '@/utils/landing'

/**
 * FR-129 B1-s — the install section's lead follows `wizardPairs`, the
 * server's `pairing` AND `wizard_pairing` (`GET /api/setup/features`, read
 * together by `useSetupFeaturesStore`): the token wording until the server
 * has said both, the browser sign-in once it has. Both homepages
 * (`LandingView.vue`, the static `/`) render this one rule.
 */
describe('downloadLead (FR-129 B1-s)', () => {
  it('names the browser sign-in only once the server pairs AND the released wizard does', () => {
    expect(downloadLead(false)).toBe(DOWNLOAD.lead)
    expect(downloadLead(true)).toBe(DOWNLOAD.leadPairing)
    expect(DOWNLOAD.leadPairing).not.toBe(DOWNLOAD.lead)
  })

  it('keeps the agreed wording, character for character (the homepage film, 2026-10-28)', () => {
    expect(DOWNLOAD.leadPairing).toBe(
      'Run Roomler Setup and sign in from your browser, with no token to copy. Or paste one command with a token from your workspace (Devices → Enroll device).',
    )
    // The token lead is what the page said before FR-129: unchanged.
    expect(DOWNLOAD.lead).toBe(
      'Paste one command, or on Windows run the graphical installer. Enrollment tokens come from your workspace (Devices → Enroll device).',
    )
  })
})
