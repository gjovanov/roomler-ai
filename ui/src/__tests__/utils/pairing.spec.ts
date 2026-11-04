// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-129 (#3022) A2 — the `/pair` page's pure parts: the user code's shape,
// the pending-pairing marker (set, expiry, consumed on use, a storage that
// throws), and the device card's time and country helpers.
import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  PENDING_PAIR_KEY,
  PENDING_PAIR_TTL_MS,
  archLabel,
  clearPendingPair,
  countryName,
  elapsedSince,
  formatElapsed,
  formatUserCode,
  hasPendingPair,
  isCompleteUserCode,
  normalizeUserCode,
  rememberPendingPair,
  takePendingPair,
  type StorageLike,
} from '@/utils/pairing'

function memoryStorage(): StorageLike & { map: Map<string, string> } {
  const map = new Map<string, string>()
  return {
    map,
    getItem: (k) => map.get(k) ?? null,
    setItem: (k, v) => void map.set(k, v),
    removeItem: (k) => void map.delete(k),
  }
}

/** A private window that refuses every access, or a storage policy that does. */
function throwingStorage(): StorageLike {
  const refuse = () => {
    throw new DOMException('The operation is insecure.', 'SecurityError')
  }
  return { getItem: refuse, setItem: refuse, removeItem: refuse }
}

describe('the user code', () => {
  it('upper-cases, drops the hyphen and anything outside the alphabet, and keeps eight letters', () => {
    expect(normalizeUserCode('bcdf-ghjk')).toBe('BCDFGHJK')
    expect(normalizeUserCode('  b c d f / g h j k l m n p ')).toBe('BCDFGHJK')
    // Vowels and digits are not in the alphabet, so a slip is dropped, not kept.
    expect(normalizeUserCode('aeiou0129')).toBe('')
    expect(normalizeUserCode('BCDF-GHJK-LMNP')).toBe('BCDFGHJK')
    expect(normalizeUserCode('')).toBe('')
  })

  it('shows the hyphen once the code passes four letters', () => {
    expect(formatUserCode('')).toBe('')
    expect(formatUserCode('BCD')).toBe('BCD')
    expect(formatUserCode('BCDF')).toBe('BCDF')
    expect(formatUserCode('BCDFG')).toBe('BCDF-G')
    expect(formatUserCode('BCDFGHJK')).toBe('BCDF-GHJK')
  })

  it('is complete at exactly eight letters', () => {
    expect(isCompleteUserCode('BCDFGHJ')).toBe(false)
    expect(isCompleteUserCode('BCDFGHJK')).toBe(true)
  })

  it('round-trips what a person types: format(normalize(x)) is stable', () => {
    const typed = 'bcdf-ghjk'
    const once = formatUserCode(normalizeUserCode(typed))
    expect(formatUserCode(normalizeUserCode(once))).toBe(once)
  })
})

describe('the pending-pairing marker', () => {
  const T0 = 1_700_000_000_000

  it('is set to now + 15 minutes, and holds until then', () => {
    const s = memoryStorage()
    rememberPendingPair(T0, s)
    expect(s.map.get(PENDING_PAIR_KEY)).toBe(String(T0 + PENDING_PAIR_TTL_MS))
    expect(PENDING_PAIR_TTL_MS).toBe(15 * 60 * 1000)
    expect(hasPendingPair(T0, s)).toBe(true)
    expect(hasPendingPair(T0 + PENDING_PAIR_TTL_MS - 1, s)).toBe(true)
  })

  it('expires at the deadline, and an expired marker is cleared on the read', () => {
    const s = memoryStorage()
    rememberPendingPair(T0, s)
    expect(hasPendingPair(T0 + PENDING_PAIR_TTL_MS, s)).toBe(false)
    expect(s.map.has(PENDING_PAIR_KEY)).toBe(false)
  })

  it('a marker that is not a number is cleared and reads false', () => {
    const s = memoryStorage()
    s.setItem(PENDING_PAIR_KEY, 'soon')
    expect(hasPendingPair(T0, s)).toBe(false)
    expect(s.map.has(PENDING_PAIR_KEY)).toBe(false)
  })

  it('is consumed on use: true once, then gone', () => {
    const s = memoryStorage()
    rememberPendingPair(T0, s)
    expect(takePendingPair(T0 + 1000, s)).toBe(true)
    expect(s.map.has(PENDING_PAIR_KEY)).toBe(false)
    expect(takePendingPair(T0 + 1000, s)).toBe(false)
  })

  it('an expired marker is not taken', () => {
    const s = memoryStorage()
    rememberPendingPair(T0, s)
    expect(takePendingPair(T0 + PENDING_PAIR_TTL_MS + 1, s)).toBe(false)
  })

  it('reads false and never throws when storage throws (private mode)', () => {
    const s = throwingStorage()
    expect(() => rememberPendingPair(T0, s)).not.toThrow()
    expect(hasPendingPair(T0, s)).toBe(false)
    expect(takePendingPair(T0, s)).toBe(false)
    expect(() => clearPendingPair(s)).not.toThrow()
  })

  it('reads false and never throws without any storage', () => {
    expect(() => rememberPendingPair(T0, null)).not.toThrow()
    expect(hasPendingPair(T0, null)).toBe(false)
    expect(takePendingPair(T0, null)).toBe(false)
  })

  describe('on the browser’s localStorage (the default)', () => {
    afterEach(() => {
      localStorage.removeItem(PENDING_PAIR_KEY)
      vi.restoreAllMocks()
    })

    it('writes, reads and clears the one key', () => {
      rememberPendingPair(T0)
      expect(localStorage.getItem(PENDING_PAIR_KEY)).toBe(String(T0 + PENDING_PAIR_TTL_MS))
      expect(hasPendingPair(T0 + 5)).toBe(true)
      expect(takePendingPair(T0 + 5)).toBe(true)
      expect(localStorage.getItem(PENDING_PAIR_KEY)).toBeNull()
    })

    it('survives a localStorage whose methods throw', () => {
      vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
        throw new DOMException('QuotaExceededError')
      })
      vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
        throw new DOMException('SecurityError')
      })
      expect(() => rememberPendingPair(T0)).not.toThrow()
      expect(hasPendingPair(T0)).toBe(false)
      expect(takePendingPair(T0)).toBe(false)
    })
  })
})

describe('the device card’s helpers', () => {
  const NOW = Date.UTC(2026, 9, 28, 12, 0, 0)

  it('elapsedSince picks the largest unit that fits, and null for a non-time', () => {
    expect(elapsedSince(null, NOW)).toBeNull()
    expect(elapsedSince(undefined, NOW)).toBeNull()
    expect(elapsedSince('', NOW)).toBeNull()
    expect(elapsedSince('not a date', NOW)).toBeNull()
    expect(elapsedSince(new Date(NOW - 10_000).toISOString(), NOW)).toEqual({ unit: 'second', value: 10 })
    expect(elapsedSince(new Date(NOW - 90_000).toISOString(), NOW)).toEqual({ unit: 'minute', value: 2 })
    expect(elapsedSince(new Date(NOW - 3 * 60_000).toISOString(), NOW)).toEqual({ unit: 'minute', value: 3 })
    expect(elapsedSince(NOW - 2 * 3_600_000, NOW)).toEqual({ unit: 'hour', value: 2 })
    expect(elapsedSince(new Date(NOW - 3 * 86_400_000), NOW)).toEqual({ unit: 'day', value: 3 })
    // A skewed clock puts the start in the future: that is "now", never negative.
    expect(elapsedSince(new Date(NOW + 60_000).toISOString(), NOW)).toEqual({ unit: 'second', value: 0 })
  })

  it('formatElapsed says "just now" under a minute, and the browser’s phrase above it', () => {
    expect(formatElapsed({ unit: 'second', value: 30 }, 'en', 'just now')).toBe('just now')
    expect(formatElapsed({ unit: 'minute', value: 3 }, 'en', 'just now')).toBe('3 minutes ago')
    expect(formatElapsed({ unit: 'hour', value: 1 }, 'en', 'just now')).toBe('1 hour ago')
    expect(formatElapsed({ unit: 'day', value: 2 }, 'en', 'just now')).toBe('2 days ago')
  })

  it('formatElapsed falls back to plain English where Intl refuses', () => {
    const real = Intl.RelativeTimeFormat
    vi.stubGlobal('Intl', {
      ...Intl,
      RelativeTimeFormat: class {
        constructor() {
          throw new RangeError('no')
        }
      },
    })
    try {
      expect(formatElapsed({ unit: 'minute', value: 3 }, 'xx', 'just now')).toBe('3 minutes ago')
      expect(formatElapsed({ unit: 'hour', value: 1 }, 'xx', 'just now')).toBe('1 hour ago')
    } finally {
      vi.unstubAllGlobals()
      expect(Intl.RelativeTimeFormat).toBe(real)
    }
  })

  it('countryName names a two-letter code, passes a name through, and is null for none', () => {
    expect(countryName(null, 'en')).toBeNull()
    expect(countryName('', 'en')).toBeNull()
    expect(countryName('  ', 'en')).toBeNull()
    expect(countryName('DE', 'en')).toBe('Germany')
    expect(countryName('de', 'en')).toBe('Germany')
    expect(countryName('Germany', 'en')).toBe('Germany')
  })

  it('archLabel spells the two common ones and leaves the rest alone', () => {
    expect(archLabel('x86_64')).toBe('x86-64')
    expect(archLabel('amd64')).toBe('x86-64')
    expect(archLabel('aarch64')).toBe('ARM64')
    expect(archLabel('arm64')).toBe('ARM64')
    expect(archLabel('riscv64')).toBe('riscv64')
    expect(archLabel(null)).toBe('')
  })
})
