// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import { VERSION_NOT_REPORTED, reportedVersion, versionLabel } from '@/utils/deviceVersion'
import { updateStallChip, type AgentReleaseBrief } from '@/utils/updateStall'

const DAY = 86_400_000
const NOW = Date.parse('2026-11-23T12:00:00Z')
// Two releases, out for days: a device on an older reported version is due its chip.
const FEED: AgentReleaseBrief[] = [
  { tag_name: 'agent-v0.4.233', published_at: new Date(NOW - 3 * DAY).toISOString() },
  { tag_name: 'agent-v0.4.232', published_at: new Date(NOW - 5 * DAY).toISOString() },
]

describe('a device row with no reported version (#3565)', () => {
  it('reads as "not reported yet", never as a version', () => {
    // The server sends "" for a row whose daemon has not said hello yet.
    for (const v of ['', '   ', null, undefined]) {
      expect(reportedVersion(v)).toBeNull()
      expect(versionLabel(v)).toBe('version not reported yet')
    }
    expect(VERSION_NOT_REPORTED).toBe('not reported yet')
  })

  it('reads a reported version as itself', () => {
    expect(reportedVersion('0.4.197')).toBe('0.4.197')
    expect(reportedVersion(' 0.4.197 ')).toBe('0.4.197')
    expect(versionLabel('0.4.197')).toBe('v0.4.197')
  })

  it('shows no "not updating" chip', () => {
    expect(updateStallChip('', true, 'windows', FEED, NOW)).toBeNull()
    expect(updateStallChip(reportedVersion(''), true, 'linux', FEED, NOW)).toBeNull()
    // The same feed gives a reported older version its chip: the quiet above is the row's.
    expect(updateStallChip('0.4.200', true, 'windows', FEED, NOW)?.text).toBe('not updating')
  })
})
