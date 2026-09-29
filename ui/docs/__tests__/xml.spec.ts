// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — the sitemap index, its urlsets, robots.txt.
 */
import { describe, expect, it } from 'vitest'
import { atomFeed, feedHtml, newest, robotsTxt, sitemapIndex, urlset, xmlEscape, type FeedInput } from '../theme/xml.ts'

/** Parsed by a real XML parser (jsdom's), so "well-formed" is checked, not assumed. */
function parseXml(xml: string): Document {
  const doc = new DOMParser().parseFromString(xml, 'application/xml')
  const err = doc.getElementsByTagName('parsererror')[0]
  if (err) throw new Error(`not well-formed: ${err.textContent}`)
  return doc
}

const FEED: FeedInput = {
  id: 'https://roomler.ai/blog/',
  selfUrl: 'https://roomler.ai/blog/feed.xml',
  htmlUrl: 'https://roomler.ai/blog/',
  title: 'Roomler blog',
  subtitle: 'Posts & notes',
  updated: '2026-09-28T20:52:50Z',
  entries: [
    {
      id: 'https://roomler.ai/blog/a/',
      url: 'https://roomler.ai/blog/a/',
      title: 'Wife’s swearing & <TeamViewer>',
      summary: 'Why "Roomler" exists',
      contentHtml: '<p>A <a href="https://roomler.ai/docs/">link</a> &amp; <code>&lt;tag&gt;</code></p>',
      published: '2026-09-25T22:45:13Z',
      updated: '2026-09-28T20:52:50Z',
      author: { name: 'Goran Jovanov', uri: 'https://github.com/gjovanov' },
      tags: ['teamviewer', 'remote-desktop'],
    },
  ],
}

describe('atomFeed', () => {
  it('is well-formed Atom, with the entry and its full content intact', () => {
    const doc = parseXml(atomFeed(FEED))
    expect(doc.documentElement.namespaceURI).toBe('http://www.w3.org/2005/Atom')
    const entry = doc.getElementsByTagName('entry')[0]!
    expect(entry.getElementsByTagName('title')[0]!.textContent).toBe('Wife’s swearing & <TeamViewer>')
    const content = entry.getElementsByTagName('content')[0]!
    expect(content.getAttribute('type')).toBe('html')
    // The HTML survives the round trip exactly: escaped once, decoded once.
    expect(content.textContent).toBe(FEED.entries[0]!.contentHtml)
    expect([...entry.getElementsByTagName('category')].map((c) => c.getAttribute('term'))).toEqual(['teamviewer', 'remote-desktop'])
  })

  it('names itself and the page it mirrors', () => {
    const doc = parseXml(atomFeed(FEED))
    const links = [...doc.documentElement.children].filter((n) => n.tagName === 'link')
    expect(links.map((l) => `${l.getAttribute('rel')} ${l.getAttribute('href')}`)).toEqual([
      'self https://roomler.ai/blog/feed.xml',
      'alternate https://roomler.ai/blog/',
    ])
  })
})

describe('feedHtml', () => {
  it('makes site links and images absolute, and leaves other URLs alone', () => {
    const out = feedHtml('<a href="/docs/x/">x</a><img src="/docs/assets/a.png"><a href="//cdn.example/y">y</a><a href="https://e.com/">e</a>', 'https://roomler.ai')
    expect(out).toContain('href="https://roomler.ai/docs/x/"')
    expect(out).toContain('src="https://roomler.ai/docs/assets/a.png"')
    expect(out).toContain('href="//cdn.example/y"')
    expect(out).toContain('href="https://e.com/"')
  })

  it("drops the page's own chrome: permalinks, copy buttons, icons", () => {
    const out = feedHtml(
      '<h2 id="a">A<a class="heading-anchor" href="#a"><svg viewBox="0 0 1 1"></svg></a></h2>' +
        '<div class="code-head"><button class="code-copy" type="button"><svg></svg></button></div>',
      'https://roomler.ai',
    )
    expect(out).toBe('<h2 id="a">A</h2><div class="code-head"></div>')
  })
})

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
