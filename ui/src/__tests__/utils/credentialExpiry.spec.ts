// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import { credentialChip } from '@/utils/credentialExpiry'

const DAY = 86_400_000
const NOW = Date.parse('2027-01-01T00:00:00Z')
const at = (days: number) => new Date(NOW + days * DAY).toISOString()

describe('credentialChip (FR-103)', () => {
  it('stays quiet for a healthy device, an unreported expiry and garbage', () => {
    expect(credentialChip(at(200), NOW)).toBeNull()
    expect(credentialChip(at(61), NOW)).toBeNull()
    expect(credentialChip(undefined, NOW)).toBeNull()
    expect(credentialChip(null, NOW)).toBeNull()
    expect(credentialChip('not a date', NOW)).toBeNull()
  })

  it('warns within 60 days: a device that is not renewing', () => {
    const chip = credentialChip(at(30), NOW)
    expect(chip?.color).toBe('warning')
    expect(chip?.text).toBe('credential expires in 30 d')
    expect(chip?.tooltip).toContain('2027-01-31')
    expect(credentialChip(at(0.5), NOW)?.text).toBe('credential expires today')
  })

  it('an expired token says whether the grace still covers it', () => {
    const recent = credentialChip(at(-10), NOW)
    expect(recent?.color).toBe('error')
    expect(recent?.tooltip).toContain('80 more day(s)')
    const old = credentialChip(at(-120), NOW)
    expect(old?.tooltip).toContain('re-enroll the device')
  })
})
