/* SPDX-License-Identifier: AGPL-3.0-only
 * Copyright (C) 2026 G ROX EOOD
 *
 * FR-88 (#1790) §3a — carry, don't store.
 *
 * A campaign link (`/blog/x/?utm_source=youtube&utm_campaign=c1`, or
 * `?ref=…`) says where a visitor came from. This copies those keys onto the
 * page's OWN sign-up, install and download links, so they reach the register
 * view, which sends them once, with the new account. Two more keys ride
 * along: `landing_path`, the page the journey began on, and `referrer_host`,
 * the HOST (never the full URL) of the site that sent the visitor, when that
 * was another site. The homepage's newsletter form gets the campaign as its
 * `source`.
 *
 * NOTHING is written to the visitor's device: no cookie, no localStorage, no
 * sessionStorage, no IndexedDB. Storing on a terminal for a non-essential
 * purpose needs consent (ePrivacy Art. 5(3)); this site has no consent banner
 * and must not need one. The keys live in links, and only in links.
 *
 * No campaign key on the URL, no change at all: that is every organic visit.
 * With one, only `href`s change, so nothing on the page moves.
 *
 * ⚠️ The keys are the register view's (`ui/src/utils/attribution.ts`), locked
 * equal by `docs/__tests__/attribution.spec.ts`. The build omits this script
 * when that file's `ATTRIBUTION_ENABLED` is false.
 */
;(function () {
  'use strict'

  var CAMPAIGN = ['utm_source', 'utm_medium', 'utm_campaign', 'utm_content', 'utm_term', 'ref']
  // The server keeps 64 characters of each value (FR-88 §3a).
  var MAX = 64

  // Where a campaign is carried to, and nowhere else: sign-up, the installer
  // downloads, and the install guides, whose own sign-up links carry it on.
  // Same origin only.
  var TARGETS = [
    /^\/register$/,
    /^\/api\/setup\/(?:windows|linux|macos)$/,
    /^\/docs\/start\/(?:quickstart|self-hosting|install\/(?:windows|macos|linux))\/$/,
  ]

  var here
  try {
    here = new URL(window.location.href)
  } catch (e) {
    return
  }

  function param(name) {
    var v = here.searchParams.get(name)
    v = v ? v.trim() : ''
    return v.slice(0, MAX)
  }

  function externalHost(referrer) {
    if (!referrer) return ''
    try {
      var host = new URL(referrer).host
      return host && host !== here.host ? host.slice(0, MAX) : ''
    } catch (e) {
      return ''
    }
  }

  function isTarget(url) {
    if (url.origin !== here.origin) return false
    // A link to this very page (`#main`, the table of contents, a heading's
    // permalink, the sidebar's current entry) is not a way onward. Carrying
    // onto it would turn an in-page jump into a reload.
    if (url.pathname === here.pathname) return false
    for (var i = 0; i < TARGETS.length; i++) {
      if (TARGETS[i].test(url.pathname)) return true
    }
    return false
  }

  var carried = []
  for (var i = 0; i < CAMPAIGN.length; i++) {
    var v = param(CAMPAIGN[i])
    if (v) carried.push([CAMPAIGN[i], v])
  }
  if (!carried.length) return

  // A page of this site that the visitor reached by a carried link already
  // knows where the journey began; only the first page works it out.
  var referrer = param('referrer_host') || externalHost(document.referrer)
  if (referrer) carried.push(['referrer_host', referrer])
  carried.push(['landing_path', param('landing_path') || here.pathname.slice(0, MAX)])

  function carry() {
    var links = document.querySelectorAll('a[href]')
    for (var i = 0; i < links.length; i++) {
      var a = links[i]
      var raw = a.getAttribute('href')
      var url
      try {
        url = new URL(raw, here)
      } catch (e) {
        continue
      }
      if (!isTarget(url)) continue
      for (var k = 0; k < carried.length; k++) {
        // A link that names its own value keeps it.
        if (!url.searchParams.has(carried[k][0])) url.searchParams.set(carried[k][0], carried[k][1])
      }
      // A site-relative href stays site-relative.
      var relative = raw.charAt(0) === '/' && raw.charAt(1) !== '/'
      a.setAttribute('href', relative ? url.pathname + url.search + url.hash : url.href)
    }

    // The newsletter form (home.js posts this as `source`), filtered as the
    // server's `clean_source` filters: a campaign that cleans to nothing
    // leaves the form's own source in place.
    var campaign = param('utm_campaign').replace(/[^A-Za-z0-9_-]/g, '').slice(0, 32)
    var form = document.querySelector('form[data-subscribe]')
    if (form && campaign) form.setAttribute('data-source', campaign)
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', carry)
  else carry()
})()
