// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — the sitemap index, its urlsets, robots.txt.
 */
import { describe, expect, it } from 'vitest'
import { newest, robotsTxt, sitemapIndex, urlset, xmlEscape } from '../theme/xml.ts'

describe('urlset', () => {
  it('writes lastmod when known and OMITS it when not — never a stand-in date', () => {
    const xml = urlset([{ loc: 'https://roomler.ai/docs/', lastmod: '2026-09-01' }, { loc: 'https://roomler.ai/docs/tags/x/' }])
    expect(xml).toContain('<loc>https://roomler.ai/docs/</loc>\n    <lastmod>2026-09-01</lastmod>')
    expect(xml).toContain('<loc>https://roomler.ai/docs/tags/x/</loc>\n  </url>')
    expect(xml.match(/<lastmod>/g)).toHaveLength(1)
  })

  it('is well-formed: the declaration, the namespace, escaped text', () => {
    const xml = urlset([{ loc: 'https://roomler.ai/a?b=1&c=2' }])
    expect(xml.startsWith('<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">')).toBe(true)
    expect(xml).toContain('a?b=1&amp;c=2')
    expect(xml.trimEnd().endsWith('</urlset>')).toBe(true)
  })
})

describe('sitemapIndex', () => {
  it('points at each collection, dated by its newest page', () => {
    const docs = [{ loc: 'x', lastmod: '2026-09-01' }, { loc: 'y', lastmod: '2026-09-20' }, { loc: 'z' }]
    const xml = sitemapIndex([{ loc: 'https://roomler.ai/sitemap-docs.xml', lastmod: newest(docs) }])
    expect(xml).toContain('<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">')
    expect(xml).toContain('<sitemap>\n    <loc>https://roomler.ai/sitemap-docs.xml</loc>\n    <lastmod>2026-09-20</lastmod>')
  })

  it('leaves a child undated when none of its pages is dated', () => {
    expect(newest([{ loc: 'x' }, { loc: 'y' }])).toBeUndefined()
  })
})

describe('robots.txt', () => {
  it('names the sitemap index and keeps the app out', () => {
    const txt = robotsTxt('https://roomler.ai')
    expect(txt).toContain('Sitemap: https://roomler.ai/sitemap.xml')
    expect(txt).toContain('Disallow: /tenant/')
    expect(txt).not.toMatch(/Disallow: \/(docs|blog)/)
  })
})

describe('xmlEscape', () => {
  it('escapes the five XML specials', () => {
    expect(xmlEscape(`<a href="x">'&'</a>`)).toBe('&lt;a href=&quot;x&quot;&gt;&apos;&amp;&apos;&lt;/a&gt;')
  })
})
