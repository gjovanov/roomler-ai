// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #1896: the image ships DB-IP's CC BY 4.0 country database, and that licence
// is conditional on a link back to DB-IP wherever its results are shown. The
// credit is a licence term, not decoration: these lock its exact wording, and
// that it follows the database the server actually loaded.
import { describe, expect, it } from 'vitest'
import { DBIP_CREDIT, GEOLITE_CREDIT, geoipCredit } from '@/utils/geoipCredit'

describe('geoipCredit', () => {
  it("credits DB-IP with the link DB-IP's licence asks for", () => {
    expect(geoipCredit('DBIP-Country-Lite')).toEqual({ text: 'IP Geolocation by DB-IP', href: 'https://db-ip.com' })
    expect(geoipCredit('DBIP-Country-Lite')).toBe(DBIP_CREDIT)
  })

  it('credits MaxMind when the deployment supplies GeoLite2 itself', () => {
    expect(geoipCredit('GeoLite2-Country')).toBe(GEOLITE_CREDIT)
    expect(GEOLITE_CREDIT.href).toBe('https://www.maxmind.com')
  })

  it('credits nobody without a database, or for a commercial one', () => {
    expect(geoipCredit(null)).toBeNull()
    expect(geoipCredit(undefined)).toBeNull()
    expect(geoipCredit('')).toBeNull()
    // MaxMind's paid GeoIP2 asks for no credit, and must not get DB-IP's.
    expect(geoipCredit('GeoIP2-Country')).toBeNull()
  })
})
