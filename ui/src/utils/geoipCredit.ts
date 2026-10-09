// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #1896: the credit a country database's licence asks for, on the page that
 * shows its results (the observability dashboard's Countries card).
 *
 * Keyed on the database the SERVER loaded (`geoip_database` in
 * `/api/admin/stats/users`, the MaxMind DB metadata's `database_type`), never
 * on what the image is expected to carry. A deployment may supply its own
 * database, or none, and must not be credited to the wrong source.
 *
 * (Not to be confused with `attribution.ts`, which is sign-up campaign
 * attribution.)
 */

export interface GeoipCredit {
  text: string
  href: string
}

/** DB-IP's Lite databases are CC BY 4.0. DB-IP asks a web application for
 *  exactly this link "on pages that display or use results from the
 *  database" (db-ip.com/db/download/ip-to-country-lite). */
export const DBIP_CREDIT: GeoipCredit = { text: 'IP Geolocation by DB-IP', href: 'https://db-ip.com' }

/** MaxMind's GeoLite EULA (§3) requires attribution; this is its own example
 *  wording, for a deployment that supplies GeoLite2 itself. */
export const GEOLITE_CREDIT: GeoipCredit = {
  text: 'This product includes GeoLite Data created by MaxMind, available from https://www.maxmind.com',
  href: 'https://www.maxmind.com',
}

/** The credit for a loaded database's `database_type`, or `null` when there is
 *  none to show: no database, or a commercial one whose licence asks for none. */
export function geoipCredit(databaseType: string | null | undefined): GeoipCredit | null {
  if (!databaseType) return null
  if (databaseType.startsWith('DBIP-')) return DBIP_CREDIT
  if (databaseType.startsWith('GeoLite')) return GEOLITE_CREDIT
  return null
}
