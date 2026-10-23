// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import { decimalBytes, rateText, relayAllowanceView } from '@/utils/relayAllowance'
import type { RelayAllowance } from '@/stores/stats'

const GB = 1e9

function q(over: Partial<RelayAllowance> = {}): RelayAllowance {
  return {
    month_start: '2026-10-01T00:00:00Z',
    resets_at: '2026-11-01T00:00:00Z',
    used_bytes: 3.2 * GB,
    allowance_bytes: 10 * GB,
    rate_kbps: 1000,
    enforced: false,
    throttled: false,
    ...over,
  }
}

describe('FR-115 P4a: relay allowance, in decimal units', () => {
  it('states bytes the way the plans are sold', () => {
    expect(decimalBytes(10 * GB)).toBe('10.0 GB')
    expect(decimalBytes(640e6)).toBe('640 MB')
    expect(decimalBytes(12_000)).toBe('12 KB')
    expect(decimalBytes(0)).toBe('0 B')
    expect(decimalBytes(Number.NaN)).toBe('0 B')
  })

  it('states the rate in Mbit/s when it is whole', () => {
    expect(rateText(1000)).toBe('1 Mbit/s')
    expect(rateText(4000)).toBe('4 Mbit/s')
    expect(rateText(512)).toBe('512 kbit/s')
  })
})

describe('FR-115 P4a: relayAllowanceView', () => {
  it('shows usage against the allowance, and nothing about a throttle that is off', () => {
    const v = relayAllowanceView(q())
    expect(v.usage).toBe('3.2 GB of 10.0 GB')
    expect(v.percent).toBe(32)
    expect(v.throttledNote).toBeNull()
    expect(v.policyNote).toBeNull()
  })

  it('says what happens past the allowance once the throttle is on', () => {
    const v = relayAllowanceView(q({ enforced: true }))
    expect(v.policyNote).toContain('slows to 1 Mbit/s')
    expect(v.policyNote).toContain('never cut off')
    expect(v.throttledNote).toBeNull()
  })

  it('warns while the org is throttled, naming the rate and the reset day', () => {
    const v = relayAllowanceView(
      q({ used_bytes: 11 * GB, enforced: true, throttled: true, rate_kbps: 4000 }),
    )
    expect(v.percent).toBe(100)
    expect(v.throttledNote).toContain('slowed to 4 Mbit/s until 1 November')
    expect(v.throttledNote).toContain('Direct connections are not affected')
    expect(v.policyNote).toBeNull()
  })

  it('says TURN counts, and that only the mesh is slowed, once the server counts it (P4c-0)', () => {
    const policy = relayAllowanceView(q({ enforced: true, counts_turn: true }))
    expect(policy.policyNote).toContain('remote desktop and meetings included')
    expect(policy.policyNote).toContain('mesh relay slows to 1 Mbit/s')
    expect(policy.policyNote).toContain('never cut off')
    const held = relayAllowanceView(
      q({ used_bytes: 11 * GB, enforced: true, throttled: true, counts_turn: true }),
    )
    expect(held.throttledNote).toContain('past its relay allowance')
    expect(held.throttledNote).toContain('slowed to 1 Mbit/s until 1 November')
    expect(held.throttledNote).toContain('remote desktop and meetings are not affected')
    // An older server sends no flag: DERP alone, the wording as before.
    const old = relayAllowanceView(q({ enforced: true }))
    expect(old.policyNote).not.toContain('remote desktop')
  })

  it('shows an unlimited plan without a bar', () => {
    const v = relayAllowanceView(q({ allowance_bytes: null, rate_kbps: null }))
    expect(v.usage).toBe('3.2 GB (unlimited)')
    expect(v.percent).toBeNull()
    expect(v.throttledNote).toBeNull()
  })
})
