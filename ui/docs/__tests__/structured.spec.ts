// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — the JSON-LD builders.
 */
import { describe, expect, it } from 'vitest'
import { ORG } from '../site.ts'
import { breadcrumbList, collectionPage, graph, jsonLdScript, organization, techArticle } from '../theme/structured.ts'

describe('the publisher', () => {
  it('is ONE Organization with the legal name every other record uses', () => {
    const org = organization()
    expect(org['@id']).toBe('https://roomler.ai/#organization')
    expect(org.legalName).toBe('G ROX EOOD')
    expect(JSON.stringify(org)).not.toContain('LTD')
  })

  it('is what every article points at, by @id', () => {
    const a = techArticle({ url: 'https://roomler.ai/docs/x/', headline: 'X', description: 'd', image: 'https://roomler.ai/i.png', keywords: [] })
    expect((a.author as { '@id': string })['@id']).toBe(ORG.id)
    expect((a.publisher as { '@id': string })['@id']).toBe(ORG.id)
  })
})

describe('techArticle', () => {
  const base = { url: 'https://roomler.ai/docs/x/', headline: 'X', description: 'd', image: 'https://roomler.ai/i.png', keywords: ['a', 'b'] }

  it('carries the dates it knows and omits the ones it does not', () => {
    expect(techArticle({ ...base, datePublished: '2026-09-01', dateModified: '2026-09-02' })).toMatchObject({
      datePublished: '2026-09-01',
      dateModified: '2026-09-02',
    })
    const undated = techArticle(base)
    expect('datePublished' in undated).toBe(false)
    expect('dateModified' in undated).toBe(false)
  })

  it('names its own page as the main entity', () => {
    expect(techArticle(base)).toMatchObject({ mainEntityOfPage: base.url, '@id': `${base.url}#article`, keywords: 'a, b' })
  })
})

describe('the rest', () => {
  it('numbers breadcrumbs from 1 with absolute URLs', () => {
    const b = breadcrumbList([{ name: 'Docs', url: '/docs/' }, { name: 'Compare', url: '/docs/compare/' }])
    expect(b.itemListElement).toEqual([
      { '@type': 'ListItem', position: 1, name: 'Docs', item: 'https://roomler.ai/docs/' },
      { '@type': 'ListItem', position: 2, name: 'Compare', item: 'https://roomler.ai/docs/compare/' },
    ])
  })

  it('describes a listing as a CollectionPage, not an article', () => {
    expect(collectionPage({ url: 'u', name: 'n', description: 'd' })['@type']).toBe('CollectionPage')
  })

  it('wraps a page in one graph', () => {
    expect(graph([organization()])).toEqual({ '@context': 'https://schema.org', '@graph': [organization()] })
  })

  it('cannot be closed early by a `</script>` inside the data', () => {
    const html = jsonLdScript({ headline: 'a </script><script>alert(1)</script>' })
    expect(html.match(/<\/script>/g)).toHaveLength(1)
    expect(html).toContain('\\u003c/script>')
  })
})
