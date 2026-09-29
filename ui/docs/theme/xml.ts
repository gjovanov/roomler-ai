// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — the site-root files a crawler reads first: the sitemap
 * index, its urlsets, and robots.txt.
 *
 * `/sitemap.xml` is an INDEX over one urlset per collection
 * (`sitemap-docs.xml`, and `sitemap-blog.xml` once a post exists), so
 * Search Console reports indexing per collection instead of as one number.
 *
 * FR-60's sitemap also listed five SPA routes (`/landing`, `/pricing`,
 * `/privacy`, `/terms`, `/imprint`) whose served canonical is `/`. Asking a
 * crawler to index a URL that names another URL as its canonical is a
 * contradiction Search Console reports as "Duplicate, submitted URL not
 * selected as canonical". They are gone.
 */

export interface UrlEntry {
  /** Absolute URL. */
  loc: string
  /** `YYYY-MM-DD`, or absent when unknown — never the build date. */
  lastmod?: string
}

export function xmlEscape(s: string): string {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&apos;')
}

function entry(tag: 'url' | 'sitemap', u: UrlEntry): string {
  return (
    `  <${tag}>\n    <loc>${xmlEscape(u.loc)}</loc>\n` +
    (u.lastmod ? `    <lastmod>${u.lastmod}</lastmod>\n` : '') +
    `  </${tag}>`
  )
}

const HEADER = `<?xml version="1.0" encoding="UTF-8"?>\n`
const NS = 'http://www.sitemaps.org/schemas/sitemap/0.9'

export function urlset(urls: UrlEntry[]): string {
  return `${HEADER}<urlset xmlns="${NS}">\n${urls.map((u) => entry('url', u)).join('\n')}\n</urlset>\n`
}

/** A child's `lastmod` is its newest page's, and absent when none is dated. */
export function newest(urls: UrlEntry[]): string | undefined {
  return urls.reduce<string | undefined>((max, u) => (u.lastmod && (!max || u.lastmod > max) ? u.lastmod : max), undefined)
}

export function sitemapIndex(children: UrlEntry[]): string {
  return `${HEADER}<sitemapindex xmlns="${NS}">\n${children.map((c) => entry('sitemap', c)).join('\n')}\n</sitemapindex>\n`
}

export function robotsTxt(origin: string): string {
  return (
    `# Roomler — ${origin}\n` +
    `User-agent: *\n` +
    `Allow: /\n` +
    `# The application itself is behind auth and client-rendered; there is\n` +
    `# nothing there for a crawler, and tenant ids should not be enumerated.\n` +
    `Disallow: /tenant/\n` +
    `Disallow: /oauth/\n` +
    `Disallow: /consent/\n` +
    `Disallow: /invite/\n` +
    `\n` +
    `Sitemap: ${origin}/sitemap.xml\n`
  )
}
