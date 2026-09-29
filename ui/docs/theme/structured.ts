// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — JSON-LD builders, shared by every page the generator
 * writes (docs now; the blog and the homepage next).
 *
 * Every page emits ONE block: `{"@context", "@graph": [...]}`, with the
 * Organization as a node of its own and every author/publisher pointing at
 * it by `@id`. One publisher, one name, everywhere.
 *
 * ⚠️ `application/ld+json` is a DATA block, not a script, so the pod CSP's
 * `script-src` does not apply to it; that is why this can be inline.
 */
import { ORG, SITE_ORIGIN } from '../site.ts'

export interface Crumb {
  name: string
  /** Site-absolute URL. */
  url: string
}

/** A reference with enough in it to stand alone if a consumer does not
 *  resolve `@id` across the graph. */
const orgRef = { '@type': 'Organization', '@id': ORG.id, name: ORG.name, url: ORG.url }

export function organization(): Record<string, unknown> {
  return {
    '@type': 'Organization',
    '@id': ORG.id,
    name: ORG.name,
    legalName: ORG.legalName,
    url: ORG.url,
    logo: { '@type': 'ImageObject', url: ORG.logo },
    sameAs: [...ORG.sameAs],
  }
}

export function breadcrumbList(trail: Crumb[]): Record<string, unknown> {
  return {
    '@type': 'BreadcrumbList',
    itemListElement: trail.map((t, i) => ({
      '@type': 'ListItem',
      position: i + 1,
      name: t.name,
      item: `${SITE_ORIGIN}${t.url}`,
    })),
  }
}

export interface ArticleInput {
  /** Absolute canonical URL. */
  url: string
  headline: string
  description: string
  /** `YYYY-MM-DD`; omitted when unknown, never guessed. */
  datePublished?: string
  dateModified?: string
  /** Absolute URL of a raster image (social platforms and Google do not take SVG). */
  image: string
  keywords: string[]
}

export function techArticle(a: ArticleInput): Record<string, unknown> {
  return {
    '@type': 'TechArticle',
    '@id': `${a.url}#article`,
    headline: a.headline,
    description: a.description,
    url: a.url,
    mainEntityOfPage: a.url,
    ...(a.datePublished ? { datePublished: a.datePublished } : {}),
    ...(a.dateModified ? { dateModified: a.dateModified } : {}),
    image: a.image,
    inLanguage: 'en',
    keywords: a.keywords.join(', '),
    author: orgRef,
    publisher: orgRef,
  }
}

/** A page whose content is a list of other pages: the docs home, a section
 *  or tag index. Not an article, so it does not claim to be one. */
export function collectionPage(p: { url: string; name: string; description: string }): Record<string, unknown> {
  return {
    '@type': 'CollectionPage',
    '@id': p.url,
    url: p.url,
    name: p.name,
    description: p.description,
    inLanguage: 'en',
    publisher: orgRef,
  }
}

export function graph(nodes: Array<Record<string, unknown>>): Record<string, unknown> {
  return { '@context': 'https://schema.org', '@graph': nodes }
}

/** `</script>` inside a JSON string would close the block early; `<` is
 *  escaped to its unicode form, which is valid JSON and inert in HTML. */
export function jsonLdScript(value: unknown): string {
  return `<script type="application/ld+json">${JSON.stringify(value).replace(/</g, '\\u003c')}</script>`
}
