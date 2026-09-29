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
import { BLOG_BASE, BLOG_TITLE, ORG, SITE_ORIGIN, type Author } from '../site.ts'

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
  /** ISO 8601, ideally with time and zone (Google flags a bare date as an
   *  invalid datetime); omitted when unknown, never guessed. */
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

/**
 * The site itself, on the homepage (P6): what Google reads for the site
 * name in results. No `SearchAction` (Google retired the sitelinks search
 * box in 2024) and no `SoftwareApplication`: that rich result requires
 * ratings or reviews, and a page with none reports it as an error — the
 * ratings are never invented to make it pass.
 */
export function website(): Record<string, unknown> {
  return {
    '@type': 'WebSite',
    '@id': `${SITE_ORIGIN}/#website`,
    url: `${SITE_ORIGIN}/`,
    name: ORG.name,
    inLanguage: 'en',
    publisher: orgRef,
  }
}

// ── the blog (P4) ────────────────────────────────────────────────────────

export const BLOG_ID = `${SITE_ORIGIN}${BLOG_BASE}/#blog`

export function person(a: Author): Record<string, unknown> {
  return { '@type': 'Person', name: a.name, url: a.url, sameAs: [...a.sameAs] }
}

export interface BlogPostingInput {
  /** Absolute canonical URL. */
  url: string
  headline: string
  description: string
  /** ISO 8601 timestamps with offsets. */
  datePublished: string
  dateModified: string
  /** The share image: raster, absolute, at least 1200 px wide. */
  image: { url: string; width: number; height: number }
  author: Author
  keywords: string[]
  /** Other copies of the same post, e.g. on Medium. */
  sameAs?: string[]
}

export function blogPosting(p: BlogPostingInput): Record<string, unknown> {
  return {
    '@type': 'BlogPosting',
    '@id': `${p.url}#article`,
    headline: p.headline,
    description: p.description,
    url: p.url,
    mainEntityOfPage: p.url,
    datePublished: p.datePublished,
    dateModified: p.dateModified,
    image: { '@type': 'ImageObject', url: p.image.url, width: p.image.width, height: p.image.height },
    inLanguage: 'en',
    keywords: p.keywords.join(', '),
    author: person(p.author),
    publisher: orgRef,
    isPartOf: { '@type': 'Blog', '@id': BLOG_ID, name: BLOG_TITLE },
    ...(p.sameAs?.length ? { sameAs: p.sameAs } : {}),
  }
}

export function blog(p: {
  url: string
  description: string
  posts: Array<{ url: string; headline: string; datePublished: string }>
}): Record<string, unknown> {
  return {
    '@type': 'Blog',
    '@id': BLOG_ID,
    url: p.url,
    name: BLOG_TITLE,
    description: p.description,
    inLanguage: 'en',
    publisher: orgRef,
    blogPost: p.posts.map((x) => ({ '@type': 'BlogPosting', headline: x.headline, url: x.url, datePublished: x.datePublished })),
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
