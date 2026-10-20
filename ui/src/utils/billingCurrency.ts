// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-115 — which currency the billing page shows and asks for at checkout.
 *
 * The server lists only the currencies on sale (`stripe.currencies`, default
 * USD alone until the live Prices carry EUR). The page starts a European
 * browser on EUR and remembers the person's choice, so a choice that is not
 * on sale here must fall back without being overwritten: the remembered EUR
 * comes back on its own once EUR is sold.
 */

export type Currency = 'usd' | 'eur'

/** The currencies the plans list, in the order they first appear. */
export function currenciesOnSale(plans: Array<{ prices?: Array<{ currency: Currency }> }>): Currency[] {
  const seen: Currency[] = []
  for (const p of plans) {
    for (const x of p.prices || []) {
      if (!seen.includes(x.currency)) seen.push(x.currency)
    }
  }
  return seen
}

/** A subscription's own currency; else the person's choice while it is on
 * sale; else the first currency that is. Nothing listed yet: the choice. */
export function shownCurrency(choice: Currency, onSale: Currency[], subscription?: Currency | null): Currency {
  if (subscription) return subscription
  return onSale.length === 0 || onSale.includes(choice) ? choice : onSale[0]
}
