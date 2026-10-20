// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import { callEndedByPlanMessage, callEndingSoonMessage } from '@/utils/meetingCap'

const NOW = Date.UTC(2026, 9, 20, 12, 0, 0)
const MIN = 60_000

describe('meeting cap messages (FR-115 P5)', () => {
  it('counts the minutes left from the end, not from the warning', () => {
    expect(callEndingSoonMessage(NOW + 5 * MIN, 60, 'free', NOW)).toBe(
      'This meeting ends in 5 minutes. Meetings on the Free plan run up to 60 minutes; upgrade for meetings with no time limit.',
    )
    // Rounded up: a warning landing a sweep period (30 s) after the line still
    // reads 5, and one that lands later counts what is really left.
    expect(callEndingSoonMessage(NOW + 4 * MIN + 30_000, 60, 'free', NOW)).toContain('ends in 5 minutes')
    expect(callEndingSoonMessage(NOW + 3 * MIN, 60, 'free', NOW)).toContain('ends in 3 minutes')
  })

  it('never says zero minutes, and uses the singular for one', () => {
    expect(callEndingSoonMessage(NOW + 20_000, 60, 'free', NOW)).toContain('ends in 1 minute.')
    expect(callEndingSoonMessage(NOW - MIN, 60, 'free', NOW)).toContain('ends in 1 minute.')
  })

  it('reads without a plan name from an older server', () => {
    expect(callEndingSoonMessage(NOW + 5 * MIN, 60, undefined, NOW)).toContain('Meetings on your plan run up to 60 minutes')
    expect(callEndedByPlanMessage(60)).toBe(
      "This meeting reached your plan's 60-minute limit. Upgrade for meetings with no time limit.",
    )
  })

  it('names the plan whose limit ended the meeting', () => {
    expect(callEndedByPlanMessage(60, 'free')).toBe(
      "This meeting reached the Free plan's 60-minute limit. Upgrade for meetings with no time limit.",
    )
  })
})
