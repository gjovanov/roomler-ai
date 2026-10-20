// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import { annualOnSale, currenciesOnSale, shownCurrency } from '@/utils/billingCurrency'

const usdOnly = [{ prices: [{ currency: 'usd' as const }] }, { prices: [{ currency: 'usd' as const }] }]
const both = [{ prices: [{ currency: 'usd' as const }, { currency: 'eur' as const }] }]

describe('billing currency (FR-115)', () => {
  it('lists the currencies the plans are on sale in, once each', () => {
    expect(currenciesOnSale(usdOnly)).toEqual(['usd'])
    expect(currenciesOnSale(both)).toEqual(['usd', 'eur'])
    expect(currenciesOnSale([{}, { prices: [] }])).toEqual([])
  })

  it('falls back from a choice that is not on sale, without changing the choice', () => {
    // A European browser starts on EUR; until EUR is sold the page shows USD.
    expect(shownCurrency('eur', ['usd'])).toBe('usd')
    // Once EUR is on sale, the same remembered choice is honoured again.
    expect(shownCurrency('eur', ['usd', 'eur'])).toBe('eur')
    expect(shownCurrency('usd', ['usd', 'eur'])).toBe('usd')
  })

  it("keeps a subscription's own currency, whatever is on sale", () => {
    expect(shownCurrency('usd', ['usd'], 'eur')).toBe('eur')
    expect(shownCurrency('eur', ['usd', 'eur'], 'usd')).toBe('usd')
  })

  it('keeps the choice while the plans have not loaded', () => {
    expect(shownCurrency('eur', [])).toBe('eur')
  })

  it('counts annual as on sale only when a PAID plan lists a year (prod, 2026-10-20)', () => {
    const free = { price_cents: 0, prices: [{ interval: 'month' }, { interval: 'year' }] }
    const proMonthly = { price_cents: 800, prices: [{ interval: 'month' }] }
    const proYearly = { price_cents: 800, prices: [{ interval: 'month' }, { interval: 'year' }] }
    // Free's yearly entry alone once put an Annual toggle on the page.
    expect(annualOnSale([free, proMonthly])).toBe(false)
    expect(annualOnSale([free, proYearly])).toBe(true)
    expect(annualOnSale([])).toBe(false)
  })
})
