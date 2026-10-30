// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import { triboolChoice, triboolWord } from '@/utils/managedTribool'

describe('managedTribool (FR-130 P1b)', () => {
  it('shows each requested word as its choice, and an absent key as no selection', () => {
    expect(triboolChoice(undefined)).toBeNull()
    expect(triboolChoice('')).toBe('default')
    expect(triboolChoice('off')).toBe('off')
    expect(triboolChoice('on')).toBe('on')
  })

  it('sends the default as a clear, and a cleared select as NOT MANAGED', () => {
    // The two mappings that would change a device silently if crossed: `''`
    // clears the device's key, while an absent key leaves it alone.
    expect(triboolWord('default')).toBe('')
    expect(triboolWord(null)).toBeUndefined()
    expect(triboolWord(undefined)).toBeUndefined()
    expect(triboolWord('off')).toBe('off')
    expect(triboolWord('on')).toBe('on')
  })

  it('reads a value the select does not offer as not managed, never as a word', () => {
    for (const stray of ['', 'true', false, 0, 'OFF', {}]) {
      expect(triboolWord(stray)).toBeUndefined()
    }
  })

  it('round-trips every request', () => {
    for (const w of [undefined, '', 'off', 'on'] as const) {
      expect(triboolWord(triboolChoice(w))).toBe(w)
    }
  })
})
