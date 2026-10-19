// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import { refusalShort, updateRefusal } from '@/utils/updateRefusal'

describe('FR-67 P7 — updateRefusal', () => {
  it('maps the two words that mean "will not act on it"', () => {
    expect(updateRefusal({ update_mode: 'primary_org' })).toBe('primary_org')
    expect(updateRefusal({ update_mode: 'off' })).toBe('auto_update_off')
  })

  it('reads a device that acts, an older agent and an unknown word as no refusal', () => {
    expect(updateRefusal({ update_mode: 'self' })).toBeNull()
    expect(updateRefusal({})).toBeNull()
    expect(updateRefusal(undefined)).toBeNull()
    expect(updateRefusal(null)).toBeNull()
    expect(updateRefusal({ update_mode: 'some-future-word' })).toBeNull()
  })

  it('names each refusal code, FR-2 included', () => {
    expect(refusalShort('primary_org')).toContain('another organization')
    expect(refusalShort('auto_update_off')).toContain('auto-update is off')
    expect(refusalShort('pin_downgrade')).toContain('downgrade')
    expect(refusalShort(undefined)).toBe('refused')
  })
})
