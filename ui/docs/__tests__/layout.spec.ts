// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — what the page shell publishes, and what it must not.
 */
import { describe, expect, it } from 'vitest'
import { renderPage, type DocPage, type SiteAssets } from '../theme/layout.ts'

const assets: SiteAssets = {
  css: '/docs/assets/docs.0123456789.css',
  js: '/docs/assets/docs.abcdefabcd.js',
  search: '/docs/assets/search.1111111111.js',
  osPreference: '/docs/assets/os-preference.2222222222.js',
  searchIndex: '/docs/assets/search-index.3333333333.json',
}

const base: DocPage = {
  slug: 'start/quickstart',
  url: '/docs/start/quickstart/',
  outFile: 'start/quickstart/index.html',
  title: 'Quickstart',
  description: 'Enroll a machine and open it in a browser tab.',
  tags: ['install'],
  order: 1,
  noindex: false,
  html: '<p>Body.</p>',
  headings: [],
  plain: 'Body.',
  faq: false,
  sourceFile: 'start/quickstart',
}
const render = (page: Partial<DocPage>) => renderPage({ nav: [], page: { ...base, ...page }, assets }, new Set())

describe('dates', () => {
  it('publishes NO date when none is known — never the build date', () => {
    const html = render({ lastmod: undefined })
    expect(html).not.toContain('dateModified')
    expect(html).not.toContain('Last updated')
    expect(html).not.toContain(new Date().toISOString().slice(0, 10))
    // The edit link survives on its own.
    expect(html).toContain('Edit this page')
  })

  it('publishes a known date in the footer and the structured data', () => {
    const html = render({ lastmod: '2026-09-01' })
    expect(html).toContain('"dateModified":"2026-09-01"')
    expect(html).toContain('Last updated <time datetime="2026-09-01">2026-09-01</time>')
  })
})

describe('assets', () => {
  it('names the hashed theme files and the hashed search index', () => {
    const html = render({})
    for (const url of Object.values(assets)) expect(html).toContain(url)
    expect(html).toContain(`data-search-index="${assets.searchIndex}"`)
    // No leftover plain names in the markup.
    expect(html).not.toMatch(/\/docs\/assets\/(docs|search|os-preference)\.(css|js)"/)
  })

  it('gives the hero its real size and loads it first', () => {
    const html = render({ hero: { url: '/docs/assets/tunnels.3e0a37ab4a.svg', width: 760, height: 400 }, heroAlt: 'A tunnel' })
    expect(html).toContain('src="/docs/assets/tunnels.3e0a37ab4a.svg" alt="A tunnel" width="760" height="400"')
    expect(html).toContain('fetchpriority="high"')
    expect(html).not.toContain('width="960"')
  })
})

describe('the 404 page', () => {
  it('declares no address and describes nothing to a crawler', () => {
    const html = render({ notFound: true, noindex: true, sourceFile: undefined, title: 'Page not found' })
    expect(html).not.toContain('rel="canonical"')
    expect(html).not.toContain('og:url')
    expect(html).not.toContain('application/ld+json')
    expect(html).toContain('<meta name="robots" content="noindex, follow">')
    expect(html).not.toContain('Edit this page')
  })

  it('leaves an ordinary page its canonical', () => {
    expect(render({})).toContain('<link rel="canonical" href="https://roomler.ai/docs/start/quickstart/">')
  })
})
